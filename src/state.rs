use std::rc::Rc;

use ahash::HashSet;
use anyhow::Result;
use bitflags::bitflags;
use everscale_types::cell::*;
use everscale_types::error::Error;
use num_bigint::BigInt;
use num_traits::Zero;

#[cfg(feature = "tracing")]
use tracing::instrument;

use crate::cont::{ControlData, ControlRegs, ExcQuitCont, OrdCont, QuitCont, RcCont};
use crate::dispatch::DispatchTable;
use crate::error::{VmError, VmException};
use crate::instr::{codepage, codepage0};
use crate::stack::{RcStackValue, Stack, StackValue};
use crate::util::OwnedCellSlice;

#[derive(Default)]
pub struct VmStateBuilder {
    pub code: OwnedCellSlice,
    pub data: Option<Cell>,
    pub stack: Vec<RcStackValue>,
    pub c7: Option<Vec<RcStackValue>>,
    pub same_c3: bool,
    pub without_push0: bool,
    pub debug: Option<Box<dyn std::fmt::Write>>,
}

impl VmStateBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn build(mut self) -> Result<VmState> {
        if self.same_c3 && !self.without_push0 {
            vm_log!("implicit PUSH 0 at start");
            self.stack.push(Rc::new(BigInt::zero()));
        }

        let quit0 = QUIT0.with(Rc::clone);
        let quit1 = QUIT1.with(Rc::clone);
        let cp = codepage0();

        Ok(VmState {
            cr: ControlRegs {
                c: [
                    Some(quit0.clone()),
                    Some(quit1.clone()),
                    Some(EXC_QUIT.with(Rc::clone)),
                    Some(if self.same_c3 {
                        Rc::new(OrdCont::simple(self.code.clone(), cp.id()))
                    } else {
                        Rc::new(QuitCont { exit_code: 11 })
                    }),
                ],
                d: [
                    Some(self.data.unwrap_or_default()),
                    Some(Cell::empty_cell()),
                ],
                c7: Some(Rc::new(self.c7.unwrap_or_default())),
            },
            code: self.code,
            stack: Rc::new(Stack { items: self.stack }),
            commited_state: None,
            steps: 0,
            quit0,
            quit1,
            gas: Default::default(), // TODO: pass gas limits as argument
            cp,
            debug: self.debug,
        })
    }

    pub fn with_debug<T: std::fmt::Write + 'static>(mut self, stderr: T) -> Self {
        self.debug = Some(Box::new(stderr));
        self
    }

    pub fn with_code<T: Into<OwnedCellSlice>>(mut self, code: T) -> Self {
        self.code = code.into();
        self
    }

    pub fn with_data(mut self, data: Cell) -> Self {
        self.data = Some(data);
        self
    }

    pub fn with_same_c3(mut self) -> Self {
        self.same_c3 = true;
        self
    }

    pub fn without_push0(mut self) -> Self {
        self.without_push0 = true;
        self
    }

    pub fn with_stack<I: IntoIterator<Item = RcStackValue>>(mut self, values: I) -> Self {
        self.stack = values.into_iter().collect();
        self
    }

    pub fn push<T: StackValue + 'static>(mut self, value: T) -> Self {
        self.stack.push(Rc::new(value));
        self
    }

    pub fn push_raw(mut self, value: RcStackValue) -> Self {
        self.stack.push(value);
        self
    }

    pub fn with_c7(mut self, c7: Vec<RcStackValue>) -> Self {
        self.c7 = Some(c7);
        self
    }
}

pub struct VmState {
    pub code: OwnedCellSlice,
    pub stack: Rc<Stack>,
    pub cr: ControlRegs,
    pub commited_state: Option<CommitedState>,
    pub steps: u64,
    pub quit0: Rc<QuitCont>,
    pub quit1: Rc<QuitCont>,
    pub gas: GasConsumer,
    pub cp: &'static DispatchTable,
    pub debug: Option<Box<dyn std::fmt::Write>>,
}

impl VmState {
    pub const MAX_DATA_DEPTH: u16 = 512;

    thread_local! {
        static EMPTY_STACK: Rc<Stack> = Rc::new(Default::default());
    }

    pub fn builder() -> VmStateBuilder {
        VmStateBuilder::default()
    }

    #[cfg_attr(
        feature = "tracing",
        instrument(
            level = "trace",
            name = "vm_step",
            fields(n = self.steps),
            skip_all,
        )
    )]
    pub fn step(&mut self) -> Result<i32> {
        self.steps += 1;
        if !self.code.range().is_data_empty() {
            // TODO: dispatch
            self.cp.dispatch(self)
        } else if !self.code.range().is_refs_empty() {
            vm_log!("implicit JMPREF");

            let next_cell = self.code.apply()?.get_reference_cloned(0)?;

            // TODO: consume implicit_jmpref_gas_price
            let code = self
                .gas
                .context()
                .load_cell(next_cell, LoadMode::Full)?
                .into();

            let cont = Rc::new(OrdCont::simple(code, self.cp.id()));
            self.jump(cont)
        } else {
            vm_log!("implicit RET");

            // TODO: consume implicit_ret_gas_price
            self.ret()
        }
    }

    pub fn run(&mut self) -> i32 {
        loop {
            let res = match self.step() {
                Ok(res) => res,
                Err(e) => {
                    vm_log!(e = ?VmException::from(&e), "handling exception: {e:?}");

                    self.steps += 1;
                    match self.throw_exception(VmException::from(&e) as i32) {
                        Ok(res) => res,
                        Err(e) => {
                            vm_log!(e = ?VmException::from(&e), "double exception: {e:?}");
                            break VmException::from(&e).as_exit_code();
                        }
                    }
                }
            };

            // TODO: handle out of gas

            if res != 0 {
                // Try commit on ~(0) and ~(-1) exit codes
                if res | 1 == -1 && !self.try_commit() {
                    vm_log!("automatic commit failed");
                    self.stack = Rc::new(Stack {
                        items: vec![Rc::new(BigInt::default())],
                    });
                    break VmException::CellOverflow.as_exit_code();
                }
                break res;
            }
        }
    }

    pub fn try_commit(&mut self) -> bool {
        if let (Some(c4), Some(c5)) = (&self.cr.d[0], &self.cr.d[1]) {
            if c4.level() == 0
                && c5.level() == 0
                && c4.repr_depth() <= Self::MAX_DATA_DEPTH
                && c5.repr_depth() <= Self::MAX_DATA_DEPTH
            {
                self.commited_state = Some(CommitedState {
                    c4: c4.clone(),
                    c5: c5.clone(),
                });
                return true;
            }
        }

        false
    }

    pub fn force_commit(&mut self) -> Result<()> {
        if self.try_commit() {
            Ok(())
        } else {
            anyhow::bail!(Error::CellOverflow)
        }
    }

    pub fn take_stack(&mut self) -> Rc<Stack> {
        std::mem::replace(&mut self.stack, Self::EMPTY_STACK.with(Rc::clone))
    }

    pub fn ref_to_cont(&self, code: Cell) -> Result<Rc<OrdCont>> {
        let code = self.gas.context().load_cell(code, LoadMode::Full)?;
        Ok(Rc::new(OrdCont::simple(code.into(), self.cp.id())))
    }

    pub fn extract_cc(
        &mut self,
        mode: SaveCr,
        stack_copy: Option<u16>,
        nargs: Option<u16>,
    ) -> Result<Rc<OrdCont>> {
        let new_stack = match stack_copy {
            Some(0) => None,
            Some(n) if n as _ != self.stack.depth() => {
                ok!(Rc::make_mut(&mut self.stack).split_top(n as _).map(Some))
            }
            _ => Some(self.take_stack()),
        };

        let mut res = OrdCont {
            code: std::mem::take(&mut self.code),
            data: ControlData {
                nargs,
                stack: Some(self.take_stack()),
                save: Default::default(),
                cp: Some(self.cp.id()),
            },
        };

        if mode.contains(SaveCr::C0) {
            res.data.save.c[0] = std::mem::replace(&mut self.cr.c[0], Some(self.quit0.clone()));
        }
        if mode.contains(SaveCr::C1) {
            res.data.save.c[1] = std::mem::replace(&mut self.cr.c[1], Some(self.quit1.clone()));
        }
        if mode.contains(SaveCr::C2) {
            res.data.save.c[2] = self.cr.c[2].take();
        }

        Ok(Rc::new(res))
    }

    pub fn throw_exception(&mut self, n: i32) -> Result<i32> {
        self.stack = Rc::new(Stack {
            items: vec![Rc::new(BigInt::zero()), Rc::new(BigInt::from(n))],
        });
        self.code = Default::default();
        // TODO: try consume exception_gas_price
        let Some(c2) = self.cr.c[2].clone() else {
            anyhow::bail!(VmError::InvalidOpcode);
        };
        self.jump(c2)
    }

    pub fn throw_exception_with_arg(&mut self, n: i32, arg: RcStackValue) -> Result<i32> {
        self.stack = Rc::new(Stack {
            items: vec![arg, Rc::new(BigInt::from(n))],
        });
        self.code = Default::default();
        // TODO: try consume exception_gas_price
        let Some(c2) = self.cr.c[2].clone() else {
            anyhow::bail!(VmError::InvalidOpcode);
        };
        self.jump(c2)
    }

    pub fn call(&mut self, mut cont: RcCont) -> Result<i32> {
        if let Some(control_data) = cont.get_control_data() {
            if control_data.save.c[0].is_some() {
                // If cont has c0 then call reduces to a jump
                return self.jump(cont);
            }
            if control_data.stack.is_some() || control_data.nargs.is_some() {
                // If cont has non-empty stack or expects a fixed number of
                // arguments, call is not simple
                return self.call_ext(cont, None, None);
            }
        }

        // Create return continuation
        let mut ret = OrdCont::simple(self.code, self.cp.id());
        ret.data.save.c[0] = self.cr.c[0].take();
        self.cr.c[0] = Some(Rc::new(ret));

        // NOTE: cont.data.save.c[0] must not be set
        cont.jump(self)
    }

    pub fn call_ext(
        &mut self,
        mut cont: RcCont,
        pass_args: Option<u16>,
        ret_args: Option<u16>,
    ) -> Result<i32> {
        if let Some(control_data) = cont.get_control_data() {
            if control_data.save.c[0].is_some() {
                // If cont has c0 then call reduces to a jump
                return self.jump(cont);
            }

            let depth = self.stack.depth();
            anyhow::ensure!(
                pass_args.unwrap_or_default() as usize <= depth
                    && control_data.nargs.unwrap_or_default() as usize <= depth,
                VmError::StackUnderflow(std::cmp::max(
                    pass_args.unwrap_or_default(),
                    control_data.nargs.unwrap_or_default()
                ) as usize)
            );

            if let Some(pass_args) = pass_args {
                anyhow::ensure!(
                    control_data.nargs.unwrap_or_default() <= pass_args,
                    VmError::StackUnderflow(pass_args as usize)
                );
            }

            let old_c0 = self.cr.c[0].take();
            self.cr.preclear(&control_data.save);

            let (copy, skip) = match (pass_args, control_data.nargs) {
                (Some(pass_args), Some(copy)) => (Some(copy), pass_args - copy),
                (Some(pass_args), None) => (Some(pass_args), 0),
                _ => (None, 0),
            };

            let new_stack = match &control_data.stack {
                Some(stack) if !stack.items.is_empty() => {
                    // TODO
                }
                _ => {
                    if let Some(copy) = copy {
                        let stack = Rc::make_mut(&mut self.stack);
                    } else {
                        std::mem::replace(&mut self.stack, Self::EMPTY_STACK.with(Rc::clone))
                    }
                }
            };
        } else {
            // Simple case without continuation data

            let new_stack = if let Some(pass_args) = pass_args {
                let Some(new_length) = self.stack.depth().checked_sub(pass_args as _) else {
                    anyhow::bail!(VmError::StackUnderflow(
                        pass_args.unwrap_or_default() as usize
                    ));
                };

                Rc::new(Stack {
                    items: Rc::make_mut(&mut self.stack)
                        .items
                        .drain(new_length..)
                        .collect::<Vec<_>>(),
                })
                // TODO: consume gas of new stack length
            } else {
                std::mem::replace(&mut self.stack, Self::EMPTY_STACK.with(Rc::clone))
            };

            // Create a new stack from the top `pass_args` items of the current stack
            let mut ret = OrdCont {
                code: self.code,
                data: ControlData {
                    save: Default::default(),
                    nargs: ret_args,
                    stack: Some(std::mem::replace(&mut self.stack, new_stack)),
                    cp: Some(self.cp.id()),
                },
            };
            ret.data.save.c[0] = self.cr.c[0].take();
            self.cr.c[0] = Some(Rc::new(ret));

            cont.jump(self)
        }
    }

    pub fn jump(&mut self, cont: RcCont) -> Result<i32> {
        if let Some(cont_data) = cont.get_control_data() {
            if cont_data.stack.is_some() || cont_data.nargs.is_some() {
                // Cont has a non-empty stack or expects a fixed number of arguments
                return self.jump_ext(cont, None);
            }
        }

        // The simplest continuation case:
        // - the continuation doesn't have its own stack
        // - `nargs` is not specified so it expects the full current stack
        //
        // So, we don't need to change anything to call it
        cont.jump(self)
    }

    pub fn jump_ext(&mut self, mut cont: RcCont, pass_args: Option<u16>) -> Result<i32> {
        // Either all values or the top n values in the current stack are
        // moved to the stack of the continuation, and only then is the
        // remainder of the current stack discarded.

        if let Some(control_data) = cont.get_control_data() {
            // n = self.stack.depth()
            // if has nargs:
            //     # From docs:
            //     n' = control_data.nargs - control_data.stack.depth()
            //     # From c++ impl:
            //     n' = control_data.nargs
            //     assert n' <= n
            // if pass_args is specified:
            //     n" = pass_args
            //     assert n" >= n'
            //
            // - n" (or n) of args are taken from the current stack
            // - n' (or n) of args are passed to the continuation

            let current_depth = self.stack.depth();
            anyhow::ensure!(
                pass_args.unwrap_or_default() as usize <= current_depth
                    && control_data.nargs.unwrap_or_default() as usize <= current_depth,
                VmError::StackUnderflow(std::cmp::max(
                    pass_args.unwrap_or_default(),
                    control_data.nargs.unwrap_or_default()
                ) as usize)
            );

            if let Some(pass_args) = pass_args {
                anyhow::ensure!(
                    control_data.nargs.unwrap_or_default() <= pass_args,
                    VmError::StackUnderflow(pass_args as usize)
                );
            }

            // Prepare the current savelist to be overwritten by the continuation
            self.preclear_cr(&control_data.save);

            // Compute the next stack depth
            let next_depth = control_data.nargs.or(pass_args).unwrap_or(current_depth) as usize;

            // Try to reuse continuation stack to reduce copies
            let cont_stack = match Rc::get_mut(&mut cont) {
                None => cont
                    .get_control_data()
                    .and_then(|control_data| control_data.stack.clone()),
                Some(cont) => cont
                    .get_control_data_mut()
                    .and_then(|control_data| control_data.stack.take()),
            };

            match cont_stack {
                // If continuation has a non-empty stack, extend it from the current stack
                Some(mut cont_stack) if !cont_stack.items.is_empty() => {
                    // TODO?: don't copy `self.stack` here
                    ok!(Rc::make_mut(&mut cont_stack)
                        .move_from_stack(Rc::make_mut(&mut self.stack), next_depth));
                    // TODO: consume_stack_gas(cont_stack)

                    self.stack = cont_stack;
                }
                // Ensure that the current stack has an exact number of items
                _ if next_depth < current_depth => {
                    ok!(Rc::make_mut(&mut self.stack).drop_bottom(current_depth - next_depth));
                    // TODO: consume_stack_gas(copy)
                }
                // Leave the current stack untouched
                _ => {}
            }
        } else if let Some(pass_args) = pass_args {
            // Try to leave only `pass_args` number of arguments in the current stack
            let Some(depth_diff) = self.stack.depth().checked_sub(pass_args as _) else {
                anyhow::bail!(VmError::StackUnderflow(pass_args as _));
            };

            if depth_diff > 0 {
                // Modify the current stack only when needed
                ok!(Rc::make_mut(&mut self.stack).drop_bottom(depth_diff));
                // TODO: consume_stack_gas(pass_args)
            }
        }

        // Proceed to the continuation
        cont.jump(self)
    }

    pub fn ret(&mut self) -> Result<i32> {
        let cont = ok!(self.take_c0());
        self.jump(cont)
    }

    pub fn ret_ext(&mut self, ret_args: Option<u16>) -> Result<i32> {
        let cont = ok!(self.take_c0());
        self.jump_ext(cont, ret_args)
    }

    pub fn ret_alt(&mut self) -> Result<i32> {
        let cont = ok!(self.take_c1());
        self.jump(cont)
    }

    pub fn ret_alt_ext(&mut self, ret_args: Option<u16>) -> Result<i32> {
        let cont = ok!(self.take_c1());
        self.jump_ext(cont, ret_args)
    }

    pub fn adjust_cr(&mut self, save: &ControlRegs) {
        self.cr.merge(save)
    }

    pub fn preclear_cr(&mut self, save: &ControlRegs) {
        self.cr.preclear(save)
    }

    pub fn set_c0(&mut self, cont: RcCont) {
        self.cr.c[0] = Some(cont);
    }

    pub fn set_code(&mut self, code: OwnedCellSlice, cp: u16) -> Result<()> {
        self.code = code;
        self.force_cp(cp)
    }

    pub fn force_cp(&mut self, cp: u16) -> Result<()> {
        let Some(cp) = codepage(cp) else {
            anyhow::bail!(VmError::InvalidOpcode);
        };
        self.cp = cp;
        Ok(())
    }

    fn take_c0(&mut self) -> Result<RcCont> {
        let Some(cont) = std::mem::replace(&mut self.cr.c[0], Some(self.quit0.clone())) else {
            anyhow::bail!(VmError::InvalidOpcode);
        };
        Ok(cont)
    }

    fn take_c1(&mut self) -> Result<RcCont> {
        let Some(cont) = std::mem::replace(&mut self.cr.c[1], Some(self.quit1.clone())) else {
            anyhow::bail!(VmError::InvalidOpcode);
        };
        Ok(cont)
    }
}

pub struct CommitedState {
    pub c4: Cell,
    pub c5: Cell,
}

bitflags! {
    pub struct SaveCr: u8 {
        const C0 = 1;
        const C1 = 1 << 1;
        const C2 = 1 << 2;

        const C0_C1 = SaveCr::C0.bits() | SaveCr::C1.bits();
        const FULL = SaveCr::C0_C1.bits() | SaveCr::C2.bits();
    }
}

// TODO: remove default
#[derive(Default)]
pub struct GasConsumer {
    pub gas_max: u64,
    pub gas_limit: u64,
    pub gas_credit: u64,
    pub gas_remaining: u64,
    pub loaded_cells: HashSet<HashBytes>,
    pub empty_context: <Cell as CellFamily>::EmptyCellContext,
}

impl GasConsumer {
    const BUILD_CELL_GAS: u64 = 500;
    const NEW_CELL_GAS: u64 = 100;
    const OLD_CELL_GAS: u64 = 25;

    pub fn try_consume(&mut self, amount: u64) -> Result<(), Error> {
        if let Some(remaining) = self.gas_remaining.checked_sub(amount) {
            self.gas_remaining = remaining;
            Ok(())
        } else {
            Err(Error::Cancelled)
        }
    }

    pub fn context(&mut self) -> GasConsumerContext<'_> {
        GasConsumerContext(self)
    }
}

pub struct GasConsumerContext<'a>(&'a mut GasConsumer);

impl GasConsumerContext<'_> {
    fn consume_load_cell(&mut self, cell: &DynCell, mode: LoadMode) -> Result<(), Error> {
        if mode.use_gas() {
            let gas = if self.0.loaded_cells.insert(*cell.repr_hash()) {
                GasConsumer::NEW_CELL_GAS
            } else {
                GasConsumer::OLD_CELL_GAS
            };
            ok!(self.0.try_consume(gas));
        }
        Ok(())
    }
}

impl CellContext for GasConsumerContext<'_> {
    fn finalize_cell(&mut self, cell: CellParts<'_>) -> Result<Cell, Error> {
        ok!(self.0.try_consume(GasConsumer::BUILD_CELL_GAS));
        self.0.empty_context.finalize_cell(cell)
    }

    fn load_cell(&mut self, cell: Cell, mode: LoadMode) -> Result<Cell, Error> {
        ok!(self.consume_load_cell(cell.as_ref(), mode));
        Ok(cell)
    }

    fn load_dyn_cell<'a>(
        &mut self,
        cell: &'a DynCell,
        mode: LoadMode,
    ) -> Result<&'a DynCell, Error> {
        ok!(self.consume_load_cell(cell, mode));
        Ok(cell)
    }
}

thread_local! {
    static QUIT0: Rc<QuitCont> = Rc::new(QuitCont { exit_code: 0 });
    static QUIT1: Rc<QuitCont> = Rc::new(QuitCont { exit_code: 1 });
    static EXC_QUIT: Rc<ExcQuitCont> = Rc::new(ExcQuitCont);
}
