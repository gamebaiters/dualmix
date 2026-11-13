using System;
using System.Runtime.InteropServices;
using System.Security;
using System.Security.Principal;
using Microsoft.Win32;

namespace DualMix.Core;

/// <summary>
/// "Start with Windows", as a task of the Windows Task Scheduler (no administrator rights needed for one's own
/// logon task). The task starts DualMix at sign-in, windowless, under the supervisor that restarts it if it ever
/// ends abnormally: the closest thing to a service for a program that needs the user's desktop (tray icon,
/// keyboard, per-user audio). It can be switched off in DualMix (Generale) or in the Task Scheduler itself.
/// </summary>
public static class AutoStart
{
    public const string TaskName = "DualMix";
    private const string LegacyRunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";

    /// <summary>The command the task runs.</summary>
    public static (string File, string Arguments) Command => AppPaths.BackgroundCommand(tray: true);

    public static bool IsEnabled => Query(out bool enabled, out _, out _) && enabled;

    /// <summary>True when the task exists, is enabled and points at this installation.</summary>
    public static bool IsCurrent
    {
        get
        {
            if (!Query(out bool enabled, out var file, out var args) || !enabled) return false;
            var (f, a) = Command;
            return string.Equals(file, f, StringComparison.OrdinalIgnoreCase) && string.Equals(args, a, StringComparison.OrdinalIgnoreCase);
        }
    }

    public static void Set(bool enabled)
    {
        RemoveLegacyRunEntry();
        try
        {
            dynamic service = Connect();
            dynamic folder = service.GetFolder("\\");
            if (enabled)
            {
                // 6 = TASK_CREATE_OR_UPDATE, 3 = TASK_LOGON_INTERACTIVE_TOKEN
                folder.RegisterTask(TaskName, BuildXml(), 6, null, null, 3, null);
                Log.Info("Autostart task registered");
            }
            else
            {
                try
                {
                    folder.DeleteTask(TaskName, 0);
                    Log.Info("Autostart task removed");
                }
                catch (Exception ex) when (ex is System.IO.FileNotFoundException or COMException)
                {
                    // was not there
                }
            }
        }
        catch (Exception ex)
        {
            Log.Error("Cannot change the autostart task", ex);
        }
    }

    private static object Connect()
    {
        var type = Type.GetTypeFromProgID("Schedule.Service") ?? throw new InvalidOperationException("Task Scheduler not available");
        dynamic service = Activator.CreateInstance(type)!;
        service.Connect();
        return service;
    }

    private static bool Query(out bool enabled, out string file, out string arguments)
    {
        enabled = false;
        file = arguments = "";
        try
        {
            dynamic service = Connect();
            dynamic task = service.GetFolder("\\").GetTask(TaskName);
            enabled = (bool)task.Enabled;
            dynamic actions = task.Definition.Actions;
            if ((int)actions.Count >= 1)
            {
                dynamic action = actions.Item(1);
                file = (string)(action.Path ?? "");
                arguments = (string)(action.Arguments ?? "");
            }
            return true;
        }
        catch
        {
            return false; // no such task
        }
    }

    private static string BuildXml()
    {
        var (file, args) = Command;
        string user = WindowsIdentity.GetCurrent().User?.Value ?? Environment.UserName;
        static string X(string s) => SecurityElement.Escape(s) ?? "";
        // Priority 4 = normal process priority (the Task Scheduler default, 7, would run the audio engine below normal).
        return $"""
            <?xml version="1.0" encoding="UTF-16"?>
            <Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
              <RegistrationInfo>
                <Description>DualMix, mixer Game/Chat: avvio all'accesso, senza finestre, con riavvio automatico in caso di errore. Si disattiva da DualMix (Generale) o da qui.</Description>
                <URI>\{TaskName}</URI>
              </RegistrationInfo>
              <Triggers>
                <LogonTrigger>
                  <Enabled>true</Enabled>
                  <UserId>{X(user)}</UserId>
                </LogonTrigger>
              </Triggers>
              <Principals>
                <Principal id="Author">
                  <UserId>{X(user)}</UserId>
                  <LogonType>InteractiveToken</LogonType>
                  <RunLevel>LeastPrivilege</RunLevel>
                </Principal>
              </Principals>
              <Settings>
                <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
                <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
                <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
                <AllowHardTerminate>true</AllowHardTerminate>
                <StartWhenAvailable>true</StartWhenAvailable>
                <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
                <IdleSettings>
                  <StopOnIdleEnd>false</StopOnIdleEnd>
                  <RestartOnIdle>false</RestartOnIdle>
                </IdleSettings>
                <AllowStartOnDemand>true</AllowStartOnDemand>
                <Enabled>true</Enabled>
                <Hidden>false</Hidden>
                <RunOnlyIfIdle>false</RunOnlyIfIdle>
                <WakeToRun>false</WakeToRun>
                <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
                <Priority>4</Priority>
              </Settings>
              <Actions Context="Author">
                <Exec>
                  <Command>{X(file)}</Command>
                  <Arguments>{X(args)}</Arguments>
                  <WorkingDirectory>{X(AppPaths.AppDir.TrimEnd('\\'))}</WorkingDirectory>
                </Exec>
              </Actions>
            </Task>
            """;
    }

    /// <summary>DualMix 2.0 started from HKCU\...\Run, which opened a console window: remove that entry.</summary>
    private static void RemoveLegacyRunEntry()
    {
        try
        {
            using var k = Registry.CurrentUser.OpenSubKey(LegacyRunKey, writable: true);
            if (k?.GetValue("DualMix") != null) k.DeleteValue("DualMix");
        }
        catch
        {
            // not critical
        }
    }
}

/// <summary>
/// Windows "Communications activity" setting (Sound control panel → Communications tab).
/// By default Windows lowers every other sound by 80% when it detects a call, which defeats the mix.
/// </summary>
public static class WindowsDucking
{
    private const string Key = @"Software\Microsoft\Multimedia\Audio";
    private const string Value = "UserDuckingPreference";

    /// <summary>0 = mute others, 1 = reduce 80% (default), 2 = reduce 50%, 3 = do nothing.</summary>
    public static int Current
    {
        get
        {
            try
            {
                using var k = Registry.CurrentUser.OpenSubKey(Key);
                return k?.GetValue(Value) is int v ? v : 1;
            }
            catch
            {
                return 1;
            }
        }
    }

    public static bool IsDisabled => Current == 3;

    public static void SetDisabled(bool disabled)
    {
        try
        {
            using var k = Registry.CurrentUser.CreateSubKey(Key);
            k.SetValue(Value, disabled ? 3 : 1, RegistryValueKind.DWord);
            Log.Info($"Windows ducking {(disabled ? "disabled" : "restored to default")}");
        }
        catch (Exception ex)
        {
            Log.Error("Cannot change ducking preference", ex);
        }
    }
}
