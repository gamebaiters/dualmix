using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Linq;
using System.Runtime.InteropServices;
using System.Threading;
using DualMix.Core;

namespace DualMix.Input;

/// <summary>A system-wide hot key to register (<see cref="KeyboardHook.SetHotkeys"/>).</summary>
public readonly record struct HotkeySpec(int Id, int Vk, HotkeyModifiers Mods, bool NoRepeat);

/// <summary>
/// Keyboard input for the knob, on its own thread with its own message loop. Two mechanisms work together:
/// <list type="bullet">
/// <item><b>System hot keys</b> (RegisterHotKey) are the main one. Windows itself takes the key away from everyone
/// else and hands it to DualMix: it works whatever window is in the foreground (also elevated ones), for the media
/// keys of a knob, and it stops the Windows volume flyout from appearing.</item>
/// <item>A <b>low-level hook</b> observes the keys (key capture in the settings, the "Prova" panel) and steps in
/// for the combinations Windows refused to register (already taken by another program).</item>
/// </list>
/// </summary>
public sealed class KeyboardHook : IDisposable
{
    public const int VK_VOLUME_MUTE = 0xAD;
    public const int VK_VOLUME_DOWN = 0xAE;
    public const int VK_VOLUME_UP = 0xAF;

    private const uint WM_HOTKEY = 0x0312;
    private const uint WM_QUIT = 0x0012;
    private const uint WM_APPLY_HOTKEYS = 0x8001; // WM_APP + 1

    /// <summary>Return true to swallow the key. Must be fast: it runs inside the system input chain.</summary>
    public delegate bool KeyHandler(int vk, bool down, bool injected);

    private readonly KeyHandler _handler;
    private readonly Action<int> _hotkeyHandler;
    private Thread? _thread;
    private volatile uint _threadId;
    private IntPtr _hook;
    private HookProc? _proc; // keep the delegate alive
    private readonly ManualResetEventSlim _ready = new(false);

    private volatile HotkeySpec[] _wanted = Array.Empty<HotkeySpec>();
    private volatile HashSet<int> _active = new();
    private volatile string[] _failed = Array.Empty<string>();
    private readonly List<int> _registeredIds = new(); // keyboard thread only

    /// <summary>
    /// Treat keys sent by other programs (AutoHotkey, macro tools, remote desktop) like real key presses in the
    /// low-level hook. System hot keys always react to them.
    /// </summary>
    public volatile bool AcceptInjected;

    public KeyboardHook(KeyHandler handler, Action<int> hotkeyHandler)
    {
        _handler = handler;
        _hotkeyHandler = hotkeyHandler;
    }

    public bool IsInstalled => _hook != IntPtr.Zero;

    /// <summary>Hot keys Windows refused to register (in use by another program), as readable names.</summary>
    public IReadOnlyList<string> FailedHotkeys => _failed;

    /// <summary>Number of hot keys currently registered.</summary>
    public int ActiveHotkeys => _active.Count;

    /// <summary>Raised (keyboard thread) after the hot keys were (re)registered.</summary>
    public event Action? HotkeysApplied;

    public void Start()
    {
        if (_thread != null) return;
        _thread = new Thread(Loop) { IsBackground = true, Name = "DualMix.Keyboard", Priority = ThreadPriority.AboveNormal };
        _thread.Start();
        _ready.Wait(2000);
    }

    /// <summary>Replaces the set of system hot keys. Thread safe; applied on the keyboard thread.</summary>
    public void SetHotkeys(IReadOnlyList<HotkeySpec> hotkeys)
    {
        _wanted = hotkeys.ToArray();
        var id = _threadId;
        if (id != 0) PostThreadMessage(id, WM_APPLY_HOTKEYS, IntPtr.Zero, IntPtr.Zero);
    }

    /// <summary>True when Windows delivers this combination as a hot key (the hook must then leave it alone).</summary>
    public bool IsHotkeyActive(int vk, HotkeyModifiers mods) => _active.Contains(Pack(vk, mods));

    private static int Pack(int vk, HotkeyModifiers mods) => ((int)mods << 16) | (vk & 0xFFFF);

    private void Loop()
    {
        // Create the message queue before anyone can post to this thread.
        PeekMessage(out _, IntPtr.Zero, 0, 0, 0);
        _proc = Callback;
        _hook = SetWindowsHookEx(13 /* WH_KEYBOARD_LL */, _proc, GetModuleHandle(null), 0);
        if (_hook == IntPtr.Zero)
            Log.Error("SetWindowsHookEx failed", new Win32Exception(Marshal.GetLastWin32Error()));
        else
            Log.Info("Keyboard hook installed");
        _threadId = GetCurrentThreadId();
        ApplyHotkeys();
        _ready.Set();

        while (GetMessage(out var msg, IntPtr.Zero, 0, 0) > 0)
        {
            if (msg.message == WM_HOTKEY)
            {
                try { _hotkeyHandler((int)msg.wParam); }
                catch (Exception ex) { Log.Error("Hot key handler failed", ex); }
                continue;
            }
            if (msg.message == WM_APPLY_HOTKEYS)
            {
                ApplyHotkeys();
                continue;
            }
            TranslateMessage(ref msg);
            DispatchMessage(ref msg);
        }

        foreach (var id in _registeredIds) UnregisterHotKey(IntPtr.Zero, id);
        _registeredIds.Clear();
        _active = new HashSet<int>();
        if (_hook != IntPtr.Zero) UnhookWindowsHookEx(_hook);
        _hook = IntPtr.Zero;
        Log.Info("Keyboard hook removed");
    }

    private void ApplyHotkeys()
    {
        foreach (var id in _registeredIds) UnregisterHotKey(IntPtr.Zero, id);
        _registeredIds.Clear();

        var wanted = _wanted;
        var active = new HashSet<int>();
        var failed = new List<string>();
        foreach (var h in wanted)
        {
            uint mods = 0;
            if (h.Mods.HasFlag(HotkeyModifiers.Alt)) mods |= 0x1;
            if (h.Mods.HasFlag(HotkeyModifiers.Ctrl)) mods |= 0x2;
            if (h.Mods.HasFlag(HotkeyModifiers.Shift)) mods |= 0x4;
            if (h.Mods.HasFlag(HotkeyModifiers.Win)) mods |= 0x8;
            if (h.NoRepeat) mods |= 0x4000;
            if (RegisterHotKey(IntPtr.Zero, h.Id, mods, (uint)h.Vk))
            {
                _registeredIds.Add(h.Id);
                active.Add(Pack(h.Vk, h.Mods));
            }
            else
            {
                failed.Add(KnobController.ChordName(h.Vk, h.Mods));
            }
        }
        _active = active;
        _failed = failed.ToArray();
        Log.Info($"Hot keys: {active.Count} of {wanted.Length} registered" +
                 (failed.Count > 0 ? $" (taken by another program, handled by the hook: {string.Join(", ", failed)})" : ""));
        try { HotkeysApplied?.Invoke(); } catch { /* UI not ready */ }
    }

    private IntPtr Callback(int nCode, IntPtr wParam, IntPtr lParam)
    {
        if (nCode >= 0)
        {
            try
            {
                var data = Marshal.PtrToStructure<KBDLLHOOKSTRUCT>(lParam);
                int msg = (int)wParam;
                bool down = msg is 0x0100 or 0x0104; // WM_KEYDOWN / WM_SYSKEYDOWN
                bool injected = (data.flags & 0x10) != 0 && !AcceptInjected; // LLKHF_INJECTED
                if (_handler((int)data.vkCode, down, injected)) return (IntPtr)1;
            }
            catch (Exception ex)
            {
                Log.Error("Keyboard hook handler failed", ex);
            }
        }
        return CallNextHookEx(IntPtr.Zero, nCode, wParam, lParam);
    }

    public void Dispose()
    {
        if (_thread == null) return;
        var id = _threadId;
        if (id != 0) PostThreadMessage(id, WM_QUIT, IntPtr.Zero, IntPtr.Zero);
        _thread.Join(1000);
        _thread = null;
    }

    public static bool IsDown(int vk) => (GetAsyncKeyState(vk) & 0x8000) != 0;

    private delegate IntPtr HookProc(int nCode, IntPtr wParam, IntPtr lParam);

    [StructLayout(LayoutKind.Sequential)]
    private struct KBDLLHOOKSTRUCT
    {
        public uint vkCode;
        public uint scanCode;
        public uint flags;
        public uint time;
        public IntPtr dwExtraInfo;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct MSG
    {
        public IntPtr hwnd;
        public uint message;
        public IntPtr wParam;
        public IntPtr lParam;
        public uint time;
        public int x, y;
    }

    [DllImport("user32.dll", SetLastError = true)]
    private static extern IntPtr SetWindowsHookEx(int idHook, HookProc lpfn, IntPtr hMod, uint dwThreadId);

    [DllImport("user32.dll")]
    private static extern bool UnhookWindowsHookEx(IntPtr hhk);

    [DllImport("user32.dll")]
    private static extern IntPtr CallNextHookEx(IntPtr hhk, int nCode, IntPtr wParam, IntPtr lParam);

    [DllImport("user32.dll")]
    private static extern int GetMessage(out MSG lpMsg, IntPtr hWnd, uint min, uint max);

    [DllImport("user32.dll")]
    private static extern bool PeekMessage(out MSG lpMsg, IntPtr hWnd, uint min, uint max, uint remove);

    [DllImport("user32.dll")]
    private static extern bool TranslateMessage(ref MSG lpMsg);

    [DllImport("user32.dll")]
    private static extern IntPtr DispatchMessage(ref MSG lpMsg);

    [DllImport("user32.dll")]
    private static extern bool PostThreadMessage(uint threadId, uint msg, IntPtr wParam, IntPtr lParam);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool RegisterHotKey(IntPtr hWnd, int id, uint fsModifiers, uint vk);

    [DllImport("user32.dll")]
    private static extern bool UnregisterHotKey(IntPtr hWnd, int id);

    [DllImport("user32.dll")]
    private static extern short GetAsyncKeyState(int vKey);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr GetModuleHandle(string? name);

    [DllImport("kernel32.dll")]
    private static extern uint GetCurrentThreadId();
}
