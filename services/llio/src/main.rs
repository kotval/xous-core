#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

mod api;
use api::*;
mod i2c;
#[cfg(any(target_os = "none", target_os = "xous"))]
mod llio_hw;
#[cfg(any(target_os = "none", target_os = "xous"))]
use llio_hw::*;

#[cfg(not(any(target_os = "none", target_os = "xous")))]
mod llio_hosted;
#[cfg(not(any(target_os = "none", target_os = "xous")))]
use llio_hosted::*;

use num_traits::*;
use xous_ipc::Buffer;
use xous::{CID, msg_scalar_unpack, msg_blocking_scalar_unpack};

use std::thread;

fn i2c_thread(i2c_sid: xous::SID) {
    let xns = xous_names::XousNames::new().unwrap();

    let handler_conn = xous::connect(i2c_sid).expect("couldn't make handler connection for i2c");
    let mut i2c = i2c::I2cStateMachine::new(handler_conn);

    // register a suspend/resume listener
    let sr_cid = xous::connect(i2c_sid).expect("couldn't create suspend callback connection");
    let mut susres = susres::Susres::new(None, &xns, I2cOpcode::SuspendResume as u32, sr_cid).expect("couldn't create suspend/resume object");

    let mut suspend_pending_token: Option<usize> = None;
    log::trace!("starting i2c main loop");
    loop {
        let mut msg = xous::receive_message(i2c_sid).unwrap();
        log::trace!("i2c message: {:?}", msg);
        match FromPrimitive::from_usize(msg.body.id()) {
            Some(I2cOpcode::SuspendResume) => xous::msg_scalar_unpack!(msg, token, _, _, _, {
                if !i2c.is_busy() {
                    i2c.suspend();
                    susres.suspend_until_resume(token).expect("couldn't execute suspend/resume");
                    i2c.resume();
                } else {
                    // stash the token, and we'll do the suspend once the I2C transaction is done.
                    suspend_pending_token = Some(token);
                }
            }),
            Some(I2cOpcode::IrqI2cTxrxWriteDone) => msg_scalar_unpack!(msg, _, _, _, _, {
                if let Some(token) = suspend_pending_token.take() {
                    i2c.suspend();
                    susres.suspend_until_resume(token).expect("couldn't execute suspend/resume");
                    i2c.resume();
                }
                // I2C state machine handler irq result
                i2c.report_write_done();
            }),
            Some(I2cOpcode::IrqI2cTxrxReadDone) => msg_scalar_unpack!(msg, _, _, _, _, {
                if let Some(token) = suspend_pending_token.take() {
                    i2c.suspend();
                    susres.suspend_until_resume(token).expect("couldn't execute suspend/resume");
                    i2c.resume();
                }
                // I2C state machine handler irq result
                i2c.report_read_done();
            }),
            Some(I2cOpcode::I2cTxRx) => {
                let mut buffer = unsafe { Buffer::from_memory_message_mut(msg.body.memory_message_mut().unwrap()) };
                let i2c_txrx = buffer.to_original::<api::I2cTransaction, _>().unwrap();
                let status = i2c.initiate(i2c_txrx);
                buffer.replace(status).unwrap();
            },
            Some(I2cOpcode::I2cIsBusy) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                let busy = if i2c.is_busy() {1} else {0};
                xous::return_scalar(msg.sender, busy as _).expect("couldn't return I2cIsBusy");
            }),
            Some(I2cOpcode::Quit) => {
                log::info!("Received quit opcode, exiting!");
                break;
            }
            None => {
                log::error!("Received unknown opcode: {:?}", msg);
            }
        }
    }
    xns.unregister_server(i2c_sid).unwrap();
    xous::destroy_server(i2c_sid).unwrap();
}


#[derive(Copy, Clone, Debug)]
struct ScalarCallback {
    server_to_cb_cid: CID,
    cb_to_client_cid: CID,
    cb_to_client_id: u32,
}

#[xous::xous_main]
fn xmain() -> ! {
    // very early on map in the GPIO base so we can have the right logging enabled
    let gpio_base = crate::log_init();

    log_server::init_wait().unwrap();
    log::set_max_level(log::LevelFilter::Info);
    log::info!("my PID is {}", xous::process::id());

    let xns = xous_names::XousNames::new().unwrap();
    // connections expected:
    // - codec
    // - GAM
    // - keyboard
    // - shellchat/sleep
    // - shellchat/environment
    // - shellchat/autoupdater
    // - spinor (for turning off wfi during writes)
    // - rootkeys (for reboots)
    // - oqc-test (for testing the vibe motor)
    // - net (for COM interrupt dispatch)
    // - pddb also allocates a connection, but then releases it, to read the DNA field.
    // We've migrated the I2C function out (which is arguably the most sensitive bit), so we can now set this more safely to unrestriced connection counts.
    let llio_sid = xns.register_name(api::SERVER_NAME_LLIO, None).expect("can't register server");
    log::trace!("registered with NS -- {:?}", llio_sid);

    // create the I2C handler thread
    // - codec
    // - rtc
    // - llio
    // I2C can be used to set time, which can have security implications; we are more strict on counting who can have access to this resource.
    let i2c_sid = xns.register_name(api::SERVER_NAME_I2C, Some(3)).expect("can't register I2C thread");
    log::trace!("registered I2C thread with NS -- {:?}", i2c_sid);
    let _ = thread::spawn({
        let i2c_sid = i2c_sid.clone();
        move || {
            i2c_thread(i2c_sid);
        }
    });

    // Create a new llio object
    let handler_conn = xous::connect(llio_sid).expect("can't create IRQ handler connection");
    let mut llio = Llio::new(handler_conn, gpio_base);
    llio.ec_power_on(); // ensure this is set correctly; if we're on, we always want the EC on.

    if cfg!(feature = "wfi_off") {
        log::warn!("WFI is overridden at boot -- automatic power savings is OFF!");
        llio.wfi_override(true);
    }

    // register a suspend/resume listener
    let sr_cid = xous::connect(llio_sid).expect("couldn't create suspend callback connection");
    let mut susres = susres::Susres::new(Some(susres::SuspendOrder::Late), &xns, Opcode::SuspendResume as u32, sr_cid).expect("couldn't create suspend/resume object");
    let mut latest_activity = 0;

    let mut usb_cb_conns: [Option<ScalarCallback>; 32] = [None; 32];
    let mut com_cb_conns: [Option<ScalarCallback>; 32] = [None; 32];
    let mut rtc_cb_conns: [Option<ScalarCallback>; 32] = [None; 32];
    let mut gpio_cb_conns: [Option<ScalarCallback>; 32] = [None; 32];

    let mut lockstatus_force_update = true; // some state to track if we've been through a susupend/resume, to help out the status thread with its UX update after a restart-from-cold

    // create a self-connection to I2C to handle the public, non-security sensitive RTC API calls
    let mut i2c = llio::I2c::new(&xns);
    let mut rtc_alarm_enabled = false;
    let mut wakeup_alarm_enabled = false;

    log::trace!("starting main loop");
    loop {
        let msg = xous::receive_message(llio_sid).unwrap();
        log::trace!("Message: {:?}", msg);
        match FromPrimitive::from_usize(msg.body.id()) {
            Some(Opcode::SuspendResume) => xous::msg_scalar_unpack!(msg, token, _, _, _, {
                llio.suspend();
                #[cfg(feature="tts")]
                llio.tts_sleep_indicate(); // this happens after the suspend call because we don't want the sleep indicator to be restored on resume
                susres.suspend_until_resume(token).expect("couldn't execute suspend/resume");
                llio.resume();
                #[cfg(feature="tts")]
                llio.vibe(VibePattern::Double);
                lockstatus_force_update = true; // notify the status bar that yes, it does need to redraw the lock status, even if the value hasn't changed since the last read
            }),
            Some(Opcode::CrgMode) => msg_scalar_unpack!(msg, _mode, _, _, _, {
                todo!("CrgMode opcode not yet implemented.");
            }),
            Some(Opcode::GpioDataOut) => msg_scalar_unpack!(msg, d, _, _, _, {
                llio.gpio_dout(d as u32);
            }),
            Some(Opcode::GpioDataIn) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                xous::return_scalar(msg.sender, llio.gpio_din() as usize).expect("couldn't return gpio data in");
            }),
            Some(Opcode::GpioDataDrive) => msg_scalar_unpack!(msg, d, _, _, _, {
                llio.gpio_drive(d as u32);
            }),
            Some(Opcode::GpioIntMask) => msg_scalar_unpack!(msg, d, _, _, _, {
                llio.gpio_int_mask(d as u32);
            }),
            Some(Opcode::GpioIntAsFalling) => msg_scalar_unpack!(msg, d, _, _, _, {
                llio.gpio_int_as_falling(d as u32);
            }),
            Some(Opcode::GpioIntPending) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                xous::return_scalar(msg.sender, llio.gpio_int_pending() as usize).expect("couldn't return gpio pending vector");
            }),
            Some(Opcode::GpioIntEna) => msg_scalar_unpack!(msg, d, _, _, _, {
                llio.gpio_int_ena(d as u32);
            }),
            Some(Opcode::DebugPowerdown) => msg_scalar_unpack!(msg, arg, _, _, _, {
                let ena = if arg == 0 {false} else {true};
                llio.debug_powerdown(ena);
            }),
            Some(Opcode::DebugWakeup) => msg_scalar_unpack!(msg, arg, _, _, _, {
                let ena = if arg == 0 {false} else {true};
                llio.debug_wakeup(ena);
            }),
            Some(Opcode::UartMux) => msg_scalar_unpack!(msg, mux, _, _, _, {
                llio.set_uart_mux(mux.into());
            }),
            Some(Opcode::InfoDna) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                let (val1, val2) = llio.get_info_dna();
                xous::return_scalar2(msg.sender, val1, val2).expect("couldn't return DNA");
            }),
            Some(Opcode::InfoGit) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                let (val1, val2) = llio.get_info_git();
                xous::return_scalar2(msg.sender, val1, val2).expect("couldn't return Git");
            }),
            Some(Opcode::InfoPlatform) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                let (val1, val2) = llio.get_info_platform();
                xous::return_scalar2(msg.sender, val1, val2).expect("couldn't return Platform");
            }),
            Some(Opcode::InfoTarget) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                let (val1, val2) = llio.get_info_target();
                xous::return_scalar2(msg.sender, val1, val2).expect("couldn't return Target");
            }),
            Some(Opcode::PowerAudio) => msg_blocking_scalar_unpack!(msg, power_on, _, _, _, {
                if power_on == 0 {
                    llio.power_audio(false);
                } else {
                    llio.power_audio(true);
                }
                xous::return_scalar(msg.sender, 0).expect("couldn't confirm audio power was set");
            }),
            Some(Opcode::PowerCrypto) => msg_blocking_scalar_unpack!(msg, power_on, _, _, _, {
                if power_on == 0 {
                    llio.power_crypto(false);
                } else {
                    llio.power_crypto(true);
                }
                xous::return_scalar(msg.sender, 0).expect("couldn't confirm crypto power was set");
            }),
            Some(Opcode::WfiOverride) => msg_blocking_scalar_unpack!(msg, override_, _, _, _, {
                if override_ == 0 {
                    llio.wfi_override(false);
                } else {
                    llio.wfi_override(true);
                }
                xous::return_scalar(msg.sender, 0).expect("couldn't confirm wfi override was updated");
            }),
            Some(Opcode::PowerCryptoStatus) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                let (_, sha, engine, force) = llio.power_crypto_status();
                let mut ret = 0;
                if sha { ret |= 1 };
                if engine { ret |= 2 };
                if force { ret |= 4 };
                xous::return_scalar(msg.sender, ret).expect("couldn't return crypto unit power status");
            }),
            Some(Opcode::PowerSelf) => msg_scalar_unpack!(msg, power_on, _, _, _, {
                if power_on == 0 {
                    llio.power_self(false);
                } else {
                    llio.power_self(true);
                }
            }),
            Some(Opcode::PowerBoostMode) => msg_scalar_unpack!(msg, power_on, _, _, _, {
                if power_on == 0 {
                    llio.power_boost_mode(false);
                } else {
                    llio.power_boost_mode(true);
                }
            }),
            Some(Opcode::EcSnoopAllow) => msg_scalar_unpack!(msg, power_on, _, _, _, {
                if power_on == 0 {
                    llio.ec_snoop_allow(false);
                } else {
                    llio.ec_snoop_allow(true);
                }
            }),
            Some(Opcode::EcReset) => msg_scalar_unpack!(msg, _, _, _, _, {
                llio.ec_reset();
            }),
            Some(Opcode::EcPowerOn) => msg_scalar_unpack!(msg, _, _, _, _, {
                llio.ec_power_on();
            }),
            Some(Opcode::SelfDestruct) => msg_scalar_unpack!(msg, code, _, _, _, {
                llio.self_destruct(code as u32);
            }),
            Some(Opcode::Vibe) => msg_scalar_unpack!(msg, pattern, _, _, _, {
                llio.vibe(pattern.into());
            }),
            Some(Opcode::AdcVbus) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                xous::return_scalar(msg.sender, llio.xadc_vbus() as _).expect("couldn't return Xadc");
            }),
            Some(Opcode::AdcVccInt) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                xous::return_scalar(msg.sender, llio.xadc_vccint() as _).expect("couldn't return Xadc");
            }),
            Some(Opcode::AdcVccAux) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                xous::return_scalar(msg.sender, llio.xadc_vccaux() as _).expect("couldn't return Xadc");
            }),
            Some(Opcode::AdcVccBram) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                xous::return_scalar(msg.sender, llio.xadc_vccbram() as _).expect("couldn't return Xadc");
            }),
            Some(Opcode::AdcUsbN) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                xous::return_scalar(msg.sender, llio.xadc_usbn() as _).expect("couldn't return Xadc");
            }),
            Some(Opcode::AdcUsbP) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                xous::return_scalar(msg.sender, llio.xadc_usbp() as _).expect("couldn't return Xadc");
            }),
            Some(Opcode::AdcTemperature) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                xous::return_scalar(msg.sender, llio.xadc_temperature() as _).expect("couldn't return Xadc");
            }),
            Some(Opcode::AdcGpio5) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                xous::return_scalar(msg.sender, llio.xadc_gpio5() as _).expect("couldn't return Xadc");
            }),
            Some(Opcode::AdcGpio2) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                xous::return_scalar(msg.sender, llio.xadc_gpio2() as _).expect("couldn't return Xadc");
            }),
            Some(Opcode::EventUsbAttachSubscribe) => {
                let buffer = unsafe { Buffer::from_memory_message(msg.body.memory_message().unwrap()) };
                let hookdata = buffer.to_original::<ScalarHook, _>().unwrap();
                do_hook(hookdata, &mut usb_cb_conns);
            }
            Some(Opcode::EventComSubscribe) => {
                let buffer = unsafe { Buffer::from_memory_message(msg.body.memory_message().unwrap()) };
                let hookdata = buffer.to_original::<ScalarHook, _>().unwrap();
                do_hook(hookdata, &mut com_cb_conns);
            }
            Some(Opcode::EventRtcSubscribe) => {
                let buffer = unsafe { Buffer::from_memory_message(msg.body.memory_message().unwrap()) };
                let hookdata = buffer.to_original::<ScalarHook, _>().unwrap();
                do_hook(hookdata, &mut rtc_cb_conns);
            }
            Some(Opcode::GpioIntSubscribe) => {
                let buffer = unsafe { Buffer::from_memory_message(msg.body.memory_message().unwrap()) };
                let hookdata = buffer.to_original::<ScalarHook, _>().unwrap();
                do_hook(hookdata, &mut gpio_cb_conns);
            }
            Some(Opcode::EventComEnable) => msg_scalar_unpack!(msg, ena, _, _, _, {
                if ena == 0 {
                    llio.com_int_ena(false);
                } else {
                    llio.com_int_ena(true);
                }
            }),
            Some(Opcode::EventRtcEnable) => msg_scalar_unpack!(msg, ena, _, _, _, {
                if ena == 0 {
                    llio.rtc_int_ena(false);
                } else {
                    llio.rtc_int_ena(true);
                }
            }),
            Some(Opcode::EventUsbAttachEnable) => msg_scalar_unpack!(msg, ena, _, _, _, {
                if ena == 0 {
                    llio.usb_int_ena(false);
                } else {
                    llio.usb_int_ena(true);
                }
            }),
            Some(Opcode::EventComHappened) => {
                send_event(&com_cb_conns, 0);
            },
            Some(Opcode::EventRtcHappened) => {
                send_event(&rtc_cb_conns, 0);
            },
            Some(Opcode::EventUsbHappened) => {
                send_event(&usb_cb_conns, 0);
            },
            Some(Opcode::GpioIntHappened) => msg_scalar_unpack!(msg, channel, _, _, _, {
                send_event(&gpio_cb_conns, channel as usize);
            }),
            Some(Opcode::EventActivityHappened) => msg_scalar_unpack!(msg, activity, _, _, _, {
                log::debug!("activity: {}", activity);
                latest_activity = activity as u32;
            }),
            Some(Opcode::GetActivity) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                #[cfg(any(target_os = "none", target_os = "xous"))]
                {
                    let period = llio.activity_get_period() as u32;
                    // log::debug!("activity/period: {}/{}, {:.2}%", latest_activity, period, (latest_activity as f32 / period as f32) * 100.0);
                    xous::return_scalar2(msg.sender, latest_activity as usize, period as usize).expect("couldn't return activity");
                }
                #[cfg(not(any(target_os = "none", target_os = "xous")))] // fake an activity
                {
                    let period = 12_000;
                    xous::return_scalar2(msg.sender, latest_activity as usize, period as usize).expect("couldn't return activity");
                    latest_activity += period / 20;
                    latest_activity %= period;
                }
            }),
            Some(Opcode::DebugUsbOp) => msg_blocking_scalar_unpack!(msg, update_req, new_state, _, _, {
                if update_req != 0 {
                    // if new_state is true (not 0), then try to lock the USB port
                    // if false, try to unlock the USB port
                    if new_state != 0 {
                        llio.set_usb_disable(true);
                    } else {
                        llio.set_usb_disable(false);
                    }
                }
                // at this point, *read back* the new state -- don't assume it "took". The readback is always based on
                // a real hardware value and not the requested value. for now, always false.
                let is_locked = if llio.get_usb_disable() {
                    1
                } else {
                    0
                };

                // this is a performance optimization. we could always redraw the status, but, instead we only redraw when
                // the status has changed. However, there is an edge case: on a resume from suspend, the status needs a redraw,
                // even if nothing has changed. Thus, we have this separate boolean we send back to force an update in the
                // case that we have just come out of a suspend.
                let force_update = if lockstatus_force_update {
                    1
                } else {
                    0
                };
                xous::return_scalar2(msg.sender, is_locked, force_update).expect("couldn't return status");
                lockstatus_force_update = false;
            }),
            Some(Opcode::SetWakeupAlarm) => msg_blocking_scalar_unpack!(msg, delay, _, _, _, {
                if delay > u8::MAX as usize {
                    log::error!("Wakeup must be no longer than {} secs in the future", u8::MAX);
                    xous::return_scalar(msg.sender, 1).expect("couldn't return to caller");
                    continue;
                }
                let seconds = delay as u8;
                wakeup_alarm_enabled = true;
                // make sure battery switchover is enabled, otherwise we won't keep time when power goes off
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_CONTROL3, &[(Control3::BATT_STD_BL_EN).bits()]).expect("RTC access error");
                // set clock units to 1 second, output pulse length to ~218ms
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_TIMERB_CLK, &[(TimerClk::CLK_1_S | TimerClk::PULSE_218_MS).bits()]).expect("RTC access error");
                // program elapsed time
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_TIMERB, &[seconds]).expect("RTC access error");
                // enable timerb countdown interrupt, also clears any prior interrupt flag
                let mut control2 = (Control2::COUNTDOWN_B_INT).bits();
                if rtc_alarm_enabled {
                    control2 |= Control2::COUNTDOWN_A_INT.bits();
                }
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_CONTROL2, &[control2]).expect("RTC access error");
                // turn on the timer proper -- the system will wakeup in 5..4..3....
                let mut config = (Config::CLKOUT_DISABLE | Config::TIMER_B_ENABLE).bits();
                if rtc_alarm_enabled {
                    config |= (Config::TIMER_A_COUNTDWN | Config::TIMERA_SECONDS_INT_PULSED).bits();
                }
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_CONFIG, &[config]).expect("RTC access error");
                xous::return_scalar(msg.sender, 0).expect("couldn't return to caller");
            }),
            Some(Opcode::ClearWakeupAlarm) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                wakeup_alarm_enabled = false;
                // make sure battery switchover is enabled, otherwise we won't keep time when power goes off
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_CONTROL3, &[(Control3::BATT_STD_BL_EN).bits()]).expect("RTC access error");
                let mut config = Config::CLKOUT_DISABLE.bits();
                if rtc_alarm_enabled {
                    config |= (Config::TIMER_A_COUNTDWN | Config::TIMERA_SECONDS_INT_PULSED).bits();
                }
                // turn off RTC wakeup timer, in case previously set
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_CONFIG, &[config]).expect("RTC access error");
                // clear my interrupts and flags
                let mut control2 = 0;
                if rtc_alarm_enabled {
                    control2 |= Control2::COUNTDOWN_A_INT.bits();
                }
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_CONTROL2, &[control2]).expect("RTC access error");
                xous::return_scalar(msg.sender, 0).expect("couldn't return to caller");
            }),
             Some(Opcode::SetRtcAlarm) => msg_blocking_scalar_unpack!(msg, delay, _, _, _, {
                if delay > u8::MAX as usize {
                    log::error!("Alarm must be no longer than {} secs in the future", u8::MAX);
                    xous::return_scalar(msg.sender, 1).expect("couldn't return to caller");
                    continue;
                }
                let seconds = delay as u8;
                rtc_alarm_enabled = true;
                // make sure battery switchover is enabled, otherwise we won't keep time when power goes off
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_CONTROL3, &[(Control3::BATT_STD_BL_EN).bits()]).expect("RTC access error");
                // set clock units to 1 second, output pulse length to ~218ms
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_TIMERA_CLK, &[(TimerClk::CLK_1_S | TimerClk::PULSE_218_MS).bits()]).expect("RTC access error");
                // program elapsed time
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_TIMERA, &[seconds]).expect("RTC access error");
                // enable timerb countdown interrupt, also clears any prior interrupt flag
                let mut control2 = (Control2::COUNTDOWN_A_INT).bits();
                if wakeup_alarm_enabled {
                    control2 |= Control2::COUNTDOWN_B_INT.bits();
                }
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_CONTROL2, &[control2]).expect("RTC access error");
                // turn on the timer proper -- interrupt in 5..4..3....
                let mut config = (Config::CLKOUT_DISABLE | Config::TIMER_A_COUNTDWN | Config::TIMERA_SECONDS_INT_PULSED).bits();
                if wakeup_alarm_enabled {
                    config |= (Config::TIMER_B_ENABLE).bits();
                }
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_CONFIG, &[config]).expect("RTC access error");
                xous::return_scalar(msg.sender, 0).expect("couldn't return to caller");
            }),
            Some(Opcode::ClearRtcAlarm) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                rtc_alarm_enabled = false;
                // turn off RTC wakeup timer, in case previously set
                let mut config = Config::CLKOUT_DISABLE.bits();
                if wakeup_alarm_enabled {
                    config |= (Config::TIMER_B_ENABLE | Config::TIMERB_INT_PULSED).bits();
                }
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_CONFIG, &[config]).expect("RTC access error");
                // clear my interrupts and flags
                let mut control2 = 0;
                if wakeup_alarm_enabled {
                    control2 |= Control2::COUNTDOWN_B_INT.bits();
                }
                i2c.i2c_write(ABRTCMC_I2C_ADR, ABRTCMC_CONTROL2, &[control2]).expect("RTC access error");
                xous::return_scalar(msg.sender, 0).expect("couldn't return to caller");
            }),
            Some(Opcode::GetRtcCount) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                // TODO_RTC: implement get_rtc count function
                xous::return_scalar2(msg.sender, 0, 0).expect("couldn't return to caller");
            }),
            Some(Opcode::GetSessionOffset) => {
                // TODO_RTC: implement GetSessionOffset
            }
            Some(Opcode::Quit) => {
                log::info!("Received quit opcode, exiting.");
                let dropconn = xous::connect(i2c_sid).unwrap();
                xous::send_message(dropconn,
                    xous::Message::new_scalar(I2cOpcode::Quit.to_usize().unwrap(), 0, 0, 0, 0)).unwrap();
                unsafe{xous::disconnect(dropconn).unwrap();}
                break;
            }
            None => {
                log::error!("couldn't convert opcode: {:?}", msg);
            }
        }
    }
    log::trace!("main loop exit, destroying servers");
    unhook(&mut com_cb_conns);
    unhook(&mut rtc_cb_conns);
    unhook(&mut usb_cb_conns);
    unhook(&mut gpio_cb_conns);
    xns.unregister_server(llio_sid).unwrap();
    xous::destroy_server(llio_sid).unwrap();
    log::trace!("quitting");
    xous::terminate_process(0)
}

fn do_hook(hookdata: ScalarHook, cb_conns: &mut [Option<ScalarCallback>; 32]) {
    let (s0, s1, s2, s3) = hookdata.sid;
    let sid = xous::SID::from_u32(s0, s1, s2, s3);
    let server_to_cb_cid = xous::connect(sid).unwrap();
    let cb_dat = Some(ScalarCallback {
        server_to_cb_cid,
        cb_to_client_cid: hookdata.cid,
        cb_to_client_id: hookdata.id,
    });
    let mut found = false;
    for entry in cb_conns.iter_mut() {
        if entry.is_none() {
            *entry = cb_dat;
            found = true;
            break;
        }
    }
    if !found {
        log::error!("ran out of space registering callback");
    }
}
fn unhook(cb_conns: &mut [Option<ScalarCallback>; 32]) {
    for entry in cb_conns.iter_mut() {
        if let Some(scb) = entry {
            xous::send_message(scb.server_to_cb_cid,
                xous::Message::new_blocking_scalar(EventCallback::Drop.to_usize().unwrap(), 0, 0, 0, 0)
            ).unwrap();
            unsafe{xous::disconnect(scb.server_to_cb_cid).unwrap();}
        }
        *entry = None;
    }
}
fn send_event(cb_conns: &[Option<ScalarCallback>; 32], which: usize) {
    for entry in cb_conns.iter() {
        if let Some(scb) = entry {
            // note that the "which" argument is only used for GPIO events, to indicate which pin had the event
            match xous::try_send_message(scb.server_to_cb_cid,
                xous::Message::new_scalar(EventCallback::Event.to_usize().unwrap(),
                   scb.cb_to_client_cid as usize, scb.cb_to_client_id as usize, which, 0)
            ) {
                Ok(_) => {},
                Err(e) => {
                    match e {
                        xous::Error::ServerQueueFull => {
                            // this triggers if an interrupt storm happens. This could be perfectly natural and/or
                            // "expected", and the "best" behavior is probably to drop the events, but leave a warning.
                            // Examples of this would be a ping flood overwhelming the network stack.
                            log::warn!("Attempted to send event, but destination queue is full. Event was dropped: {:?}", scb);
                        }
                        xous::Error::ServerNotFound => {
                            log::warn!("Event callback subscriber has died. Event was dropped: {:?}", scb);
                        }
                        _ => {
                            log::error!("Callback error {:?}: {:?}", e, scb);
                        }
                    }
                }
            }
        };
    }
}
