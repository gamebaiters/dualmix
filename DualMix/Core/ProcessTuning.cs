using System;
using System.Diagnostics;
using System.Runtime.InteropServices;

namespace DualMix.Core;

/// <summary>
/// Keeps Windows from treating DualMix as an unimportant background program. It has no visible window most of the
/// time, which is exactly the kind of process Windows 11 slows down to save power ("efficiency mode": threads moved
/// to slow cores, timers coalesced). For a program that feeds the sound card every 10 ms that means stutter.
/// </summary>
public static class ProcessTuning
{
    public static void Apply()
    {
        try
        {
            using var p = Process.GetCurrentProcess();
            p.PriorityClass = ProcessPriorityClass.High;
            p.PriorityBoostEnabled = true;
        }
        catch (Exception ex)
        {
            Log.Warn("Cannot raise the process priority", ex);
        }

        try
        {
            // Opt out of execution-speed throttling (EcoQoS) and of timer-resolution throttling.
            var state = new PROCESS_POWER_THROTTLING_STATE
            {
                Version = 1,
                ControlMask = 0x1 | 0x4, // EXECUTION_SPEED | IGNORE_TIMER_RESOLUTION
                StateMask = 0,           // 0 for a controlled bit = "never throttle"
            };
            if (!SetProcessInformation(GetCurrentProcess(), 4 /* ProcessPowerThrottling */, ref state, Marshal.SizeOf<PROCESS_POWER_THROTTLING_STATE>()))
            {
                // Older Windows 10 builds do not know the timer-resolution bit.
                state.ControlMask = 0x1;
                SetProcessInformation(GetCurrentProcess(), 4, ref state, Marshal.SizeOf<PROCESS_POWER_THROTTLING_STATE>());
            }
        }
        catch (Exception ex)
        {
            Log.Warn("Cannot disable power throttling", ex);
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct PROCESS_POWER_THROTTLING_STATE
    {
        public uint Version;
        public uint ControlMask;
        public uint StateMask;
    }

    [DllImport("kernel32.dll")]
    private static extern IntPtr GetCurrentProcess();

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetProcessInformation(IntPtr process, int informationClass, ref PROCESS_POWER_THROTTLING_STATE info, int size);
}
