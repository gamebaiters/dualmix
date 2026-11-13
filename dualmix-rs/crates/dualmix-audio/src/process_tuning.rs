//! Process scheduling and Windows power-throttling opt-out (EcoQoS off + MMCSS Pro Audio).

#[cfg(windows)]
pub fn apply_process_tuning() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, HIGH_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetPriorityClass, SetProcessInformation, SetProcessPriorityBoost,
    };

    unsafe {
        let proc_h = GetCurrentProcess();
        let _ = SetPriorityClass(proc_h, HIGH_PRIORITY_CLASS);
        let _ = SetProcessPriorityBoost(proc_h, false); // false = enable boost

        let mut state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        if SetProcessInformation(
            proc_h,
            ProcessPowerThrottling,
            &mut state as *mut _ as *mut _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
        .is_err()
        {
            state.ControlMask = PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
            let _ = SetProcessInformation(
                proc_h,
                ProcessPowerThrottling,
                &mut state as *mut _ as *mut _,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }
}

#[cfg(not(windows))]
pub fn apply_process_tuning() {}

/// Guard that promotes the calling audio thread to MMCSS `"Pro Audio"` with critical priority.
pub struct MmcssGuard {
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
    pub active: bool,
}

impl MmcssGuard {
    #[cfg(windows)]
    pub fn enter_pro_audio() -> Self {
        use windows::Win32::System::Threading::{
            AVRT_PRIORITY_CRITICAL, AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority,
        };
        use windows::core::w;

        let mut task_idx = 0u32;
        let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_idx) };
        match handle {
            Ok(h) if !h.is_invalid() => {
                unsafe {
                    let _ = AvSetMmThreadPriority(h, AVRT_PRIORITY_CRITICAL);
                }
                Self { handle: h, active: true }
            }
            _ => Self {
                handle: windows::Win32::Foundation::HANDLE::default(),
                active: false,
            },
        }
    }

    #[cfg(not(windows))]
    pub fn enter_pro_audio() -> Self {
        Self { active: false }
    }
}

#[cfg(windows)]
impl Drop for MmcssGuard {
    fn drop(&mut self) {
        if self.active && !self.handle.is_invalid() {
            unsafe {
                let _ = windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.handle);
            }
        }
    }
}
