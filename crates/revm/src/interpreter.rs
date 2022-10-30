pub mod bytecode;
mod contract;
pub(crate) mod memory;
mod stack;

pub use bytecode::{Bytecode, BytecodeLocked, BytecodeState};
pub use contract::Contract;
use hashbrown::HashMap;
pub use memory::Memory;
pub use stack::Stack;

use crate::{
    instructions::{eval, Return},
    Gas, Host, Spec, USE_GAS, OpCode, opcode,
};
use bytes::Bytes;
use core::ops::Range;

pub const STACK_LIMIT: u64 = 1024;
pub const CALL_STACK_LIMIT: u64 = 1024;

const NGRAM: usize = 0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interpreter {
    /// Contract information and invoking data
    pub contract: Contract,
    /// Instruction pointer.
    pub instruction_pointer: *const u8,
    /// Memory.
    pub memory: Memory,
    /// Stack.
    pub stack: Stack,
    /// left gas. Memory gas can be found in Memory field.
    pub gas: Gas,
    /// After call returns, its return data is saved here.
    pub return_data_buffer: Bytes,
    /// Return value.
    pub return_range: Range<usize>,
    /// Memory limit. See [`crate::CfgEnv`].
    #[cfg(feature = "memory_limit")]
    pub memory_limit: u64,

    // Execution trace n-grams
    opcode_window: u64, // Last 8 opcodes
    opcode_counts: HashMap<u64, u64>,
}

impl Interpreter {
    pub fn current_opcode(&self) -> u8 {
        unsafe { *self.instruction_pointer }
    }
    #[cfg(not(feature = "memory_limit"))]
    pub fn new<SPEC: Spec>(contract: Contract, gas_limit: u64) -> Self {
        Self {
            instruction_pointer: contract.bytecode.as_ptr(),
            return_range: Range::default(),
            memory: Memory::new(),
            stack: Stack::new(),
            return_data_buffer: Bytes::new(),
            contract,
            gas: Gas::new(gas_limit),

            opcode_window: 0,
            opcode_counts: HashMap::new(),
        }
    }

    #[cfg(feature = "memory_limit")]
    pub fn new_with_memory_limit<SPEC: Spec>(
        contract: Contract,
        gas_limit: u64,
        memory_limit: u64,
    ) -> Self {
        Self {
            instruction_pointer: contract.bytecode.as_ptr(),
            return_range: Range::default(),
            memory: Memory::new(),
            stack: Stack::new(),
            return_data_buffer: Bytes::new(),
            contract,
            gas: Gas::new(gas_limit),
            memory_limit,

            opcode_window: 0,
            opcode_counts: HashMap::new(),
        }
    }

    pub fn contract(&self) -> &Contract {
        &self.contract
    }

    pub fn gas(&self) -> &Gas {
        &self.gas
    }

    /// Reference of interp stack.
    pub fn stack(&self) -> &Stack {
        &self.stack
    }

    pub fn add_next_gas_block(&mut self, pc: usize) -> Return {
        if USE_GAS {
            let gas_block = self.contract.gas_block(pc);
            if !self.gas.record_cost(gas_block) {
                return Return::OutOfGas;
            }
        }
        Return::Continue
    }

    /// Return a reference of the program counter.
    pub fn program_counter(&self) -> usize {
        // Safety: this is just subtraction of pointers, it is safe to do.
        unsafe {
            self.instruction_pointer
                .offset_from(self.contract.bytecode.as_ptr()) as usize
        }
    }

    /// Analyse bytecode
    pub fn analyse(&mut self) {
        for i in 0..self.contract.bytecode.bytecode().len() - 4 {
            let bytecode = self.contract.bytecode.bytecode();
            let opcode = bytecode[i];
            let target0 = bytecode[i + 1];
            let target1 = bytecode[i + 2];
            let target = ((target0 as usize) << 8) + target1 as usize;
            let next = bytecode[i + 3];
            if opcode == opcode::PUSH2 && next == opcode::JUMPI {
                if self.contract.is_valid_jump(target) {
                    // dbg!((i, target));
                    self.contract.bytecode.bytecode_mut()[i] = opcode::PUSH2_JUMPI;
                }
            }
        }
    }

    /// loop steps until we are finished with execution
    pub fn run<H: Host, SPEC: Spec>(&mut self, host: &mut H) -> Return {

        self.analyse();

        //let timer = std::time::Instant::now();
        let mut ret = Return::Continue;
        // add first gas_block
        if USE_GAS && !self.gas.record_cost(self.contract.first_gas_block()) {
            return Return::OutOfGas;
        }
        while ret == Return::Continue {
            // step
            if H::INSPECT {
                let ret = host.step(self, SPEC::IS_STATIC_CALL);
                if ret != Return::Continue {
                    return ret;
                }
            }
            let opcode = unsafe { *self.instruction_pointer };

            if NGRAM > 0 {
                self.opcode_window <<= 8;
                self.opcode_window |= opcode as u64;

                const NGRAM_MASK: u64 = (1 << (NGRAM * 8)) - 1;
                let key = self.opcode_window & NGRAM_MASK;
                if let Some(x) = self.opcode_counts.get_mut(&key) {
                    *x += 1;
                } else {
                    self.opcode_counts.insert(key, 1);
                }
            }

            // Safety: In analysis we are doing padding of bytecode so that we are sure that last.
            // byte instruction is STOP so we are safe to just increment program_counter bcs on last instruction
            // it will do noop and just stop execution of this contract
            self.instruction_pointer = unsafe { self.instruction_pointer.offset(1) };
            ret = eval::<H, SPEC>(opcode, self, host);

            if H::INSPECT {
                let ret = host.step_end(self, SPEC::IS_STATIC_CALL, ret);
                if ret != Return::Continue {
                    return ret;
                }
            }
        }
        ret
    }

    /// Copy and get the return value of the interp, if any.
    pub fn return_value(&self) -> Bytes {
        // if start is usize max it means that our return len is zero and we need to return empty
        if self.return_range.start == usize::MAX {
            Bytes::new()
        } else {
            Bytes::copy_from_slice(self.memory.get_slice(
                self.return_range.start,
                self.return_range.end - self.return_range.start,
            ))
        }
    }

    pub fn dump(&self) {
        if NGRAM == 0 {
            return;
        }
        let mut table = Vec::new();
        for (opcodes, count) in self.opcode_counts.iter() {
            table.push((*opcodes, *count));
        }
        table.sort_by_key(|entry| entry.1);
        table.reverse();
        for (opcodes, count) in table {
            for opcode in opcodes.to_be_bytes().iter().skip(8 - NGRAM) {
                print!("{:12}", OpCode::try_from_u8(*opcode).unwrap().as_str());
            }
            println!("{count:6}");
        }
    }
}
