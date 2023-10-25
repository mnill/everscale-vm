use std::rc::Rc;

use anyhow::Result;
use everscale_vm_proc::vm_module;
use num_bigint::BigInt;

use crate::dispatch::Opcodes;
use crate::error::VmError;
use crate::VmState;

pub struct Arithops;

#[vm_module]
impl Arithops {
    // === Int constants ===

    #[init]
    fn init_int_const_ext(&self, t: &mut Opcodes) -> Result<()> {
        t.add_ext_range(0x82 << 5, (0x82 << 5) + 31, 13, exec_push_int)?;
        Ok(())
    }

    #[instr(code = "7x", fmt = "PUSHINT {x}", args(x = ((args as i32 + 5) & 0xf) - 5))]
    #[instr(code = "80xx", fmt = "PUSHINT {x}", args(x = args as i8 as i32))]
    #[instr(code = "81xxxx", fmt = "PUSHINT {x}", args(x = args as i16 as i32))]
    fn exec_push_tinyint4(st: &mut VmState, x: i32) -> Result<i32> {
        ok!(Rc::make_mut(&mut st.stack).push_int(x));
        Ok(0)
    }

    fn exec_push_int(st: &mut VmState, args: u32, bits: u16) -> Result<i32> {
        let l = (args as u16 & 0b11111) + 2;
        let value_len = 3 + l * 8;
        anyhow::ensure!(
            st.code.range().has_remaining(bits + value_len, 0),
            VmError::InvalidOpcode
        );
        st.code.range_mut().try_advance(bits, 0);

        let mut bytes = [0u8; 33];
        let rem = value_len % 8;
        let mut int = {
            let mut cs = st.code.apply()?;
            let bytes = cs.load_raw(&mut bytes, value_len)?;
            st.code.set_range(cs.range());
            num_bigint::BigUint::from_bytes_be(bytes)
        };
        if rem != 0 {
            int >>= 8 - rem;
        }
        vm_log!("execute PUSHINT {int}");

        ok!(Rc::make_mut(&mut st.stack).push_int(int));

        Ok(0)
    }

    #[instr(code = "83xx", range_to = "83ff", fmt = "PUSHPOW2 {x}", args(x = (args & 0xff) + 1))]
    pub fn exec_push_pow2(st: &mut VmState, x: u32) -> Result<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        ok!(stack.push_int(BigInt::from(1) << x));
        Ok(0)
    }

    #[instr(code = "83ff", fmt = "PUSHNAN")]
    fn exec_push_nan(st: &mut VmState) -> Result<i32> {
        ok!(Rc::make_mut(&mut st.stack).push_nan());
        Ok(0)
    }

    #[instr(code = "84xx", fmt = "PUSHPOW2DEC {x}", args(x = (args & 0xff) + 1))]
    fn exec_push_pow2dec(st: &mut VmState, x: u32) -> Result<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        let mut value = BigInt::from(1) << x;
        value -= 1;
        ok!(stack.push_int(value));
        Ok(0)
    }

    #[instr(code = "85xx", fmt = "PUSHNEGPOW2 {x}", args(x = (args & 0xff) + 1))]
    fn exec_push_negpow2(st: &mut VmState, x: u32) -> Result<i32> {
        ok!(Rc::make_mut(&mut st.stack).push_int(-(BigInt::from(1) << x)));
        Ok(0)
    }

    // === Simple math instructions ===
    #[instr(code = "a0", fmt = "ADD", args(quiet = false))]
    #[instr(code = "b7a0", fmt = "QADD", args(quiet = true))]
    fn exec_add(st: &mut VmState, quiet: bool) -> Result<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        let y = ok!(stack.pop_int_or_nan());
        let x = ok!(stack.pop_int_or_nan());
        match (x, y) {
            (Some(mut x), Some(y)) => {
                *Rc::make_mut(&mut x) += y.as_ref();
                ok!(stack.push_raw_int(x, quiet));
            }
            _ if quiet => ok!(stack.push_nan()),
            _ => anyhow::bail!(VmError::IntegerOverflow),
        }
        Ok(0)
    }

    #[instr(code = "a1", fmt = "SUB", args(quiet = false))]
    #[instr(code = "b7a1", fmt = "QSUB", args(quiet = true))]
    fn exec_sub(st: &mut VmState, quiet: bool) -> Result<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        let y = ok!(stack.pop_int_or_nan());
        let x = ok!(stack.pop_int_or_nan());
        match (x, y) {
            (Some(mut x), Some(y)) => {
                *Rc::make_mut(&mut x) -= y.as_ref();
                ok!(stack.push_raw_int(x, quiet));
            }
            _ if quiet => ok!(stack.push_nan()),
            _ => anyhow::bail!(VmError::IntegerOverflow),
        }
        Ok(0)
    }

    #[instr(code = "a2", fmt = "SUBR", args(quiet = false))]
    #[instr(code = "b7a2", fmt = "QSUBR", args(quiet = true))]
    fn exec_subr(st: &mut VmState, quiet: bool) -> Result<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        let y = ok!(stack.pop_int_or_nan());
        let x = ok!(stack.pop_int_or_nan());
        match (x, y) {
            (Some(x), Some(mut y)) => {
                *Rc::make_mut(&mut y) -= x.as_ref();
                ok!(stack.push_raw_int(y, quiet));
            }
            _ if quiet => ok!(stack.push_nan()),
            _ => anyhow::bail!(VmError::IntegerOverflow),
        }
        Ok(0)
    }

    #[instr(code = "a3", fmt = "NEGATE", args(quiet = false))]
    #[instr(code = "b7a3", fmt = "QNEGATE", args(quiet = true))]
    fn exec_negate(st: &mut VmState, quiet: bool) -> Result<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        match ok!(stack.pop_int_or_nan()) {
            Some(mut x) => {
                {
                    let x = Rc::make_mut(&mut x);
                    *x = -std::mem::take(x);
                }
                ok!(stack.push_raw_int(x, quiet));
            }
            _ if quiet => ok!(stack.push_nan()),
            _ => anyhow::bail!(VmError::IntegerOverflow),
        }
        Ok(0)
    }

    #[instr(code = "a4", fmt = "INC", args(quiet = false))]
    #[instr(code = "b7a4", fmt = "QINC", args(quiet = true))]
    fn exec_inc(st: &mut VmState, quiet: bool) -> Result<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        match ok!(stack.pop_int_or_nan()) {
            Some(mut x) => {
                *Rc::make_mut(&mut x) += 1;
                ok!(stack.push_raw_int(x, quiet));
            }
            _ if quiet => ok!(stack.push_nan()),
            _ => anyhow::bail!(VmError::IntegerOverflow),
        }
        Ok(0)
    }

    #[instr(code = "a5", fmt = "DEC", args(quiet = false))]
    #[instr(code = "b7a5", fmt = "QDEC", args(quiet = true))]
    fn exec_dec(st: &mut VmState, quiet: bool) -> Result<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        match ok!(stack.pop_int_or_nan()) {
            Some(mut x) => {
                *Rc::make_mut(&mut x) -= 1;
                ok!(stack.push_raw_int(x, quiet));
            }
            _ if quiet => ok!(stack.push_nan()),
            _ => anyhow::bail!(VmError::IntegerOverflow),
        }
        Ok(0)
    }

    #[instr(code = "a6yy", fmt = "ADDINT {y}", args(y = args as i8, quiet = false))]
    #[instr(code = "b7a6yy", fmt = "QADDINT {y}", args(y = args as i8, quiet = true))]
    fn exec_addint(st: &mut VmState, y: i8, quiet: bool) -> Result<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        match ok!(stack.pop_int_or_nan()) {
            Some(mut x) => {
                *Rc::make_mut(&mut x) += y;
                ok!(stack.push_raw_int(x, quiet));
            }
            _ if quiet => ok!(stack.push_nan()),
            _ => anyhow::bail!(VmError::IntegerOverflow),
        }
        Ok(0)
    }

    #[instr(code = "a7yy", fmt = "MULINT {y}", args(y = args as i8, quiet = false))]
    #[instr(code = "b7a7yy", fmt = "QMULINT {y}", args(y = args as i8, quiet = true))]
    fn exec_mulint(st: &mut VmState, y: i8, quiet: bool) -> Result<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        match ok!(stack.pop_int_or_nan()) {
            Some(mut x) => {
                *Rc::make_mut(&mut x) *= y;
                ok!(stack.push_raw_int(x, quiet));
            }
            _ if quiet => ok!(stack.push_nan()),
            _ => anyhow::bail!(VmError::IntegerOverflow),
        }
        Ok(0)
    }

    #[instr(code = "a8", fmt = "MUL", args(quiet = false))]
    #[instr(code = "b7a8", fmt = "QMUL", args(quiet = true))]
    fn exec_mul(st: &mut VmState, quiet: bool) -> Result<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        let y = ok!(stack.pop_int_or_nan());
        let x = ok!(stack.pop_int_or_nan());
        match (x, y) {
            (Some(mut x), Some(y)) => {
                *Rc::make_mut(&mut x) *= y.as_ref();
                ok!(stack.push_raw_int(x, quiet));
            }
            _ if quiet => ok!(stack.push_nan()),
            _ => anyhow::bail!(VmError::IntegerOverflow),
        }
        Ok(0)
    }
}
