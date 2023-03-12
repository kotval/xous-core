use gdbstub::common::Signal;
use gdbstub::conn::Connection;
use gdbstub::stub::state_machine::GdbStubStateMachine;
use gdbstub::stub::{GdbStubBuilder, GdbStubError, MultiThreadStopReason};
use gdbstub::target::Target;

use crate::io::SerialRead;
use crate::platform::precursor::gdbuart::GdbUart;

mod breakpoints;
mod extended_mode;
mod monitor;
mod multi_thread_base;
mod multi_thread_resume;
mod single_register_access;
mod target;

pub struct XousTarget {
    pid: Option<xous_kernel::PID>,

    /// When doing a `stepi` we patch the instruction with an illegal instruction
    /// and store the previous value here.
    patched_instruction: usize,
}

pub struct XousDebugState<'a> {
    pub target: XousTarget,
    pub server: GdbStubStateMachine<'a, XousTarget, crate::platform::precursor::gdbuart::GdbUart>,
}

static mut GDB_STATE: Option<XousDebugState> = None;
static mut GDB_BUFFER: [u8; 4096] = [0u8; 4096];

trait ProcessPid {
    fn pid(&self) -> Option<xous_kernel::PID>;
}

impl ProcessPid for XousTarget {
    fn pid(&self) -> Option<xous_kernel::PID> {
        self.pid
    }
}

fn receive_irq(uart: &mut GdbUart) {
    while let Some(c) = uart.getc() {
        process_character(c);
    }

    // If the GDB server goes away for some reason, reconstitute it
    unsafe {
        if GDB_STATE.is_none() {
            init();
        }
    }
}

impl XousTarget {
    pub fn new() -> XousTarget {
        XousTarget {
            pid: None,
            patched_instruction: 0,
        }
    }
}

fn state_can_accept_characters<'a, T: Target + ProcessPid, C: Connection>(
    machine: &GdbStubStateMachine<'a, T, C>,
) -> bool {
    match machine {
        GdbStubStateMachine::Idle(_) | GdbStubStateMachine::Running(_) => true,
        GdbStubStateMachine::CtrlCInterrupt(_) | GdbStubStateMachine::Disconnected(_) => false,
    }
}

fn ensure_can_accept_characters_inner<'a, T: Target + ProcessPid, C: Connection>(
    machine: GdbStubStateMachine<'a, T, C>,
    target: &mut T,
    recurse_count: usize,
) -> Option<GdbStubStateMachine<'a, T, C>> {
    if recurse_count == 0 {
        return None;
    }

    match machine {
        GdbStubStateMachine::Idle(_) | GdbStubStateMachine::Running(_) => Some(machine),
        GdbStubStateMachine::CtrlCInterrupt(gdb_stm_inner) => {
            if let Some(pid) = target.pid() {
                // println!("Starting debug on process {:?}", pid);
                crate::services::SystemServices::with_mut(|system_services| {
                    system_services.pause_process_for_debug(pid).unwrap()
                });
            } else {
                println!("No process specified! Not debugging");
            }

            let Ok(new_server) = gdb_stm_inner.interrupt_handled(target, Some(MultiThreadStopReason::Signal(Signal::SIGINT))) else {
                return None
            };
            ensure_can_accept_characters_inner(new_server, target, recurse_count - 1)
        }
        GdbStubStateMachine::Disconnected(gdb_stm_inner) => {
            println!(
                "GdbStubStateMachine::Disconnected due to {:?}",
                gdb_stm_inner.get_reason()
            );
            ensure_can_accept_characters_inner(
                gdb_stm_inner.return_to_idle(),
                target,
                recurse_count - 1,
            )
        }
    }
}

fn ensure_can_accept_characters<'a, T: Target + ProcessPid, C: Connection>(
    machine: GdbStubStateMachine<'a, T, C>,
    target: &mut T,
) -> Option<GdbStubStateMachine<'a, T, C>> {
    ensure_can_accept_characters_inner(machine, target, 4)
}

/// Advance the GDB state.
///
/// Two states accept characters:
///
///     GdbStubStateMachine::Idle
///     GdbStubStateMachine::Running
///
/// Two states exist merely to transition to other states:
///
///     GdbStubStateMachine::CtrlCInterrupt
///     GdbStubStateMachine::Disconnected
fn process_character(byte: u8) {
    let XousDebugState { mut target, server } = unsafe {
        GDB_STATE.take().unwrap_or_else(|| {
            init();
            GDB_STATE.take().unwrap()
        })
    };

    if !state_can_accept_characters(&server) {
        println!("GDB server was not in a state to accept characters");
        return;
    }

    let new_server = match server {
        GdbStubStateMachine::Idle(gdb_stm_inner) => {
            let Ok(gdb) = gdb_stm_inner.incoming_data(&mut target, byte).map_err(|e| println!("gdbstub error during idle operation: {:?}", e)) else {
                        return;
            };
            gdb
        }

        GdbStubStateMachine::Running(gdb_stm_inner) => {
            // If we're here we were running but have stopped now (either
            // because we hit Ctrl+c in gdb and hence got a serial interrupt
            // or we hit a breakpoint).

            match gdb_stm_inner.incoming_data(&mut target, byte) {
                Ok(pumped_stm) => pumped_stm,
                Err(GdbStubError::TargetError(e)) => {
                    println!("Target raised a fatal error: {:?}", e);
                    return;
                }
                Err(e) => {
                    println!("gdbstub error in DeferredStopReason.pump: {:?}", e);
                    return;
                }
            }
        }

        _ => {
            println!("GDB is in an unexpected state!");
            return;
        }
    };

    let Some(server) = ensure_can_accept_characters(new_server, &mut target) else {
        println!("Couldn't convert GDB into a state that accepts characters");
        return;
    };

    unsafe { GDB_STATE = Some(XousDebugState { target, server }) };
}

pub fn report_terminated(pid: xous_kernel::PID) {
    println!("Reporting that process {:?} has terminated!", pid);
    let Some(XousDebugState {
        mut target,
        server: gdb,
    }) = (unsafe { GDB_STATE.take() }) else {
        println!("No GDB!");
        return;
    };

    let new_gdb = match gdb {
        GdbStubStateMachine::Running(inner) => {
            println!("Reporting a STOP");
            match inner.report_stop(
                &mut target,
                MultiThreadStopReason::Signal(Signal::EXC_BAD_ACCESS),
            ) {
                Ok(new_gdb) => new_gdb,
                Err(e) => {
                    println!("Unable to report stop: {:?}", e);
                    return;
                }
            }
        }
        GdbStubStateMachine::CtrlCInterrupt(_inner) => {
            println!("GDB state was in CtrlCInterrupt, which shouldn't be possible!");
            return;
        }
        GdbStubStateMachine::Disconnected(_inner) => {
            println!("GDB state was in Disconnect, which shouldn't be possible!");
            return;
        }
        GdbStubStateMachine::Idle(_inner) => {
            println!("GDB state was in Idle, which shouldn't be possible!");
            return;
        }
    };

    unsafe {
        GDB_STATE = Some(XousDebugState {
            target,
            server: new_gdb,
        })
    };
}

pub fn init() {
    let uart = GdbUart::new(receive_irq).unwrap();
    let mut target = XousTarget::new();

    let server = GdbStubBuilder::new(uart)
        .with_packet_buffer(unsafe { &mut GDB_BUFFER })
        .build()
        .expect("unable to build gdb server")
        .run_state_machine(&mut target)
        .expect("unable to start gdb state machine");
    unsafe {
        GDB_STATE = Some(XousDebugState { target, server });
    }
}
