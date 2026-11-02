// V2-G13a measurement helpers (tool code, not product code). ASCII only: Windows PowerShell 5.1 reads
// UTF-8 without BOM as GBK. Loaded by perf-lib.ps1 through Add-Type.
//
// What it does:
//  - MeasureStartup: start an exe, poll (5 ms) for its top-level window, report when the window is
//    first visible, first responding, and first showing real content (PrintWindow capture with many
//    distinct colours).
//  - Tree helpers: descendants of a process, private working set (the "active private working set"
//    Task Manager column) summed over a process tree, with a per-group breakdown.
//  - A background sampler that writes one CSV row per interval.
//  - KillTree: only ever used on a pid we started ourselves.
//
// No server address, user name or user path is read, logged or written by this file.
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Imaging;
using System.IO;
using System.Management;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class PerfNative
{
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc p, IntPtr l);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr h, StringBuilder s, int max);
    [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] static extern IntPtr SendMessageTimeout(IntPtr h, uint msg, IntPtr w, IntPtr l, uint flags, uint timeout, out IntPtr result);
    [DllImport("user32.dll")] static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);

    const uint WM_NULL = 0;
    const uint WM_CLOSE = 0x0010;
    const uint SMTO_ABORTIFHUNG = 0x0002;
    const uint SWP_NOZORDER = 0x0004;
    const uint SWP_NOACTIVATE = 0x0010;

    // ---------------------------------------------------------------- process tree

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct PROCESSENTRY32
    {
        public uint dwSize, cntUsage, th32ProcessID;
        public IntPtr th32DefaultHeapID;
        public uint th32ModuleID, cntThreads, th32ParentProcessID;
        public int pcPriClassBase;
        public uint dwFlags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string szExeFile;
    }
    [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint pid);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)] static extern bool Process32First(IntPtr s, ref PROCESSENTRY32 e);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)] static extern bool Process32Next(IntPtr s, ref PROCESSENTRY32 e);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);

    /// pid -> (parent pid, exe name) for every process.
    static Dictionary<int, KeyValuePair<int, string>> Snapshot()
    {
        var map = new Dictionary<int, KeyValuePair<int, string>>();
        IntPtr snap = CreateToolhelp32Snapshot(2, 0);
        if (snap == IntPtr.Zero || snap == new IntPtr(-1)) return map;
        try
        {
            var e = new PROCESSENTRY32();
            e.dwSize = (uint)Marshal.SizeOf(typeof(PROCESSENTRY32));
            if (!Process32First(snap, ref e)) return map;
            do { map[(int)e.th32ProcessID] = new KeyValuePair<int, string>((int)e.th32ParentProcessID, e.szExeFile); }
            while (Process32Next(snap, ref e));
        }
        finally { CloseHandle(snap); }
        return map;
    }

    /// The root and all its descendants (pid -> exe name). Pid reuse could in theory add a stranger whose
    /// parent field equals a dead tree member; the start-time check in KillTree guards the kill path.
    public static Dictionary<int, string> Tree(int root)
    {
        var all = Snapshot();
        var result = new Dictionary<int, string>();
        if (!all.ContainsKey(root)) return result;
        result[root] = all[root].Value;
        bool grew = true;
        while (grew)
        {
            grew = false;
            foreach (var kv in all)
            {
                if (!result.ContainsKey(kv.Key) && result.ContainsKey(kv.Value.Key) && kv.Key != kv.Value.Key)
                {
                    result[kv.Key] = kv.Value.Value;
                    grew = true;
                }
            }
        }
        return result;
    }

    /// Kill a process tree that WE started. `rootStart` is the root's start time taken right after launch;
    /// if the pid now belongs to a different process nothing is killed.
    public static int KillTree(int root, DateTime rootStart)
    {
        int killed = 0;
        try
        {
            var p = Process.GetProcessById(root);
            if (Math.Abs((p.StartTime - rootStart).TotalSeconds) > 2) return 0;
        }
        catch { return 0; }
        var tree = Tree(root);
        foreach (var pid in tree.Keys)
        {
            try { Process.GetProcessById(pid).Kill(); killed++; } catch { }
        }
        return killed;
    }

    // ---------------------------------------------------------------- memory

    static PerformanceCounterCategory procCategory;

    /// pid -> private working set bytes from the "Process" performance counters ("Working Set - Private", the
    /// figure behind Task Manager's "Memory" column), read in one pass (about 35 ms after a slow first call).
    /// Two cheaper routes were tried and dropped: WMI ran into its provider quota after ~25 queries, and
    /// GetProcessMemoryInfo cannot open WebView2's sandboxed child processes.
    public static Dictionary<int, long> PrivateWorkingSet(ICollection<int> pids)
    {
        var result = new Dictionary<int, long>();
        if (procCategory == null) procCategory = new PerformanceCounterCategory("Process");
        var data = procCategory.ReadCategory();
        var ids = data["ID Process"];
        var ws = data["Working Set - Private"];
        var wanted = new HashSet<int>(pids);
        foreach (string inst in ids.Keys)
        {
            int pid = (int)ids[inst].RawValue;
            if (wanted.Contains(pid)) result[pid] = ws[inst].RawValue;
        }
        return result;
    }

    /// Cross-check helper: the same figure from WMI (one query, used only by the self-check).
    public static long PrivateWorkingSetWmi(int pid)
    {
        using (var s = new ManagementObjectSearcher("SELECT WorkingSetPrivate FROM Win32_PerfRawData_PerfProc_Process WHERE IDProcess = " + pid))
        using (var coll = s.Get())
        {
            foreach (ManagementObject o in coll) return Convert.ToInt64(o["WorkingSetPrivate"]);
        }
        return -1;
    }

    public class MemSample
    {
        public double RootMB, WebViewMB, OtherMB, TotalMB, CpuSeconds;
        public int Procs;
        public string Detail;
    }

    /// Sum the tree's private working set. Group "root" = the launched exe, "webview" = msedgewebview2.exe
    /// descendants, "other" = anything else (for example the MCP helper).
    public static MemSample SampleTree(int root)
    {
        var tree = Tree(root);
        var ws = PrivateWorkingSet(tree.Keys);
        var m = new MemSample();
        var sb = new StringBuilder();
        foreach (var kv in tree)
        {
            long b;
            if (!ws.TryGetValue(kv.Key, out b)) continue;
            double mb = b / 1048576.0;
            if (kv.Key == root) m.RootMB += mb;
            else if (kv.Value.Equals("msedgewebview2.exe", StringComparison.OrdinalIgnoreCase)) m.WebViewMB += mb;
            else m.OtherMB += mb;
            m.Procs++;
            try { m.CpuSeconds += Process.GetProcessById(kv.Key).TotalProcessorTime.TotalSeconds; } catch { }
            if (sb.Length > 0) sb.Append('|');
            sb.Append(kv.Value).Append('=').Append(mb.ToString("F1"));
        }
        m.TotalMB = m.RootMB + m.WebViewMB + m.OtherMB;
        m.Detail = sb.ToString();
        return m;
    }

    static Thread samplerThread;
    static volatile bool samplerStop;
    static double samplerPeak;

    /// Background sampler: one CSV row per interval (t_s, root, webview, other, total, procs), plus the peak.
    public static void StartSampler(int root, int intervalMs, string csvPath)
    {
        samplerStop = false;
        samplerPeak = 0;
        samplerThread = new Thread(() =>
        {
            var clock = Stopwatch.StartNew();
            using (var w = new StreamWriter(csvPath, false, new UTF8Encoding(false)))
            {
                w.WriteLine("epoch_ms,t_s,root_mb,webview_mb,other_mb,total_mb,procs,cpu_s");
                long next = 0;
                while (!samplerStop)
                {
                    MemSample m;
                    try { m = SampleTree(root); } catch { Thread.Sleep(200); continue; }
                    if (m.Procs == 0) break;
                    if (m.TotalMB > samplerPeak) samplerPeak = m.TotalMB;
                    w.WriteLine(string.Format("{0},{1:F2},{2:F1},{3:F1},{4:F1},{5:F1},{6},{7:F2}",
                        DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(), clock.ElapsedMilliseconds / 1000.0, m.RootMB, m.WebViewMB, m.OtherMB, m.TotalMB, m.Procs, m.CpuSeconds));
                    w.Flush();
                    next += intervalMs;
                    long wait = next - clock.ElapsedMilliseconds;
                    if (wait > 0) Thread.Sleep((int)wait);
                }
            }
        });
        samplerThread.IsBackground = true;
        samplerThread.Start();
    }

    public static double StopSampler()
    {
        samplerStop = true;
        if (samplerThread != null) samplerThread.Join(5000);
        return samplerPeak;
    }

    // ---------------------------------------------------------------- windows

    public static IntPtr FindMainWindow(int pid, int minW, int minH, out string title)
    {
        IntPtr best = IntPtr.Zero;
        long bestArea = 0;
        string bestTitle = "";
        EnumWindows((h, l) =>
        {
            uint p; GetWindowThreadProcessId(h, out p);
            if (p != (uint)pid || !IsWindowVisible(h)) return true;
            RECT r; GetWindowRect(h, out r);
            int w = r.Right - r.Left, ht = r.Bottom - r.Top;
            if (w < minW || ht < minH) return true;
            long area = (long)w * ht;
            if (area > bestArea)
            {
                bestArea = area; best = h;
                var sb = new StringBuilder(256); GetWindowText(h, sb, 256); bestTitle = sb.ToString();
            }
            return true;
        }, IntPtr.Zero);
        title = bestTitle;
        return best;
    }

    public static bool Responding(IntPtr h, uint timeoutMs)
    {
        IntPtr res;
        return SendMessageTimeout(h, WM_NULL, IntPtr.Zero, IntPtr.Zero, SMTO_ABORTIFHUNG, timeoutMs, out res) != IntPtr.Zero;
    }

    public static string WindowSize(IntPtr h)
    {
        RECT r; GetWindowRect(h, out r);
        return (r.Right - r.Left) + "x" + (r.Bottom - r.Top);
    }

    public static void MoveSize(IntPtr h, int x, int y, int w, int ht)
    {
        SetWindowPos(h, IntPtr.Zero, x, y, w, ht, SWP_NOZORDER | SWP_NOACTIVATE);
    }

    public static void CloseWindow(IntPtr h) { PostMessage(h, WM_CLOSE, IntPtr.Zero, IntPtr.Zero); }

    /// Capture the window with PrintWindow (PW_RENDERFULLCONTENT) and count distinct colours on a sparse grid.
    /// Optionally save the capture as PNG (the caller decides whether it is safe to keep).
    public static int DistinctColours(IntPtr h, string savePng)
    {
        RECT r; GetWindowRect(h, out r);
        int w = r.Right - r.Left, ht = r.Bottom - r.Top;
        if (w <= 0 || ht <= 0) return 0;
        using (var bmp = new Bitmap(w, ht, PixelFormat.Format32bppArgb))
        {
            using (var g = Graphics.FromImage(bmp))
            {
                IntPtr hdc = g.GetHdc();
                try { PrintWindow(h, hdc, 2); } finally { g.ReleaseHdc(hdc); }
            }
            var set = new HashSet<int>();
            var data = bmp.LockBits(new Rectangle(0, 0, w, ht), ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
            try
            {
                unsafeless(data, w, ht, set);
            }
            finally { bmp.UnlockBits(data); }
            if (!string.IsNullOrEmpty(savePng)) bmp.Save(savePng, ImageFormat.Png);
            return set.Count;
        }
    }

    static void unsafeless(BitmapData data, int w, int ht, HashSet<int> set)
    {
        int stride = data.Stride;
        byte[] row = new byte[Math.Abs(stride)];
        for (int y = 0; y < ht; y += 7)
        {
            Marshal.Copy(new IntPtr(data.Scan0.ToInt64() + (long)y * stride), row, 0, Math.Min(row.Length, w * 4));
            for (int x = 0; x < w; x += 5)
            {
                int i = x * 4;
                set.Add(row[i] | (row[i + 1] << 8) | (row[i + 2] << 16));
            }
        }
    }

    // ---------------------------------------------------------------- startup measurement

    public class StartupResult
    {
        public int Pid;
        public DateTime StartTime;
        public double VisibleMs = -1, RespondingMs = -1, ContentMs = -1;
        public string WindowTitle = "", WindowSize = "";
        public int Colours;
    }

    /// Start `exe` with the given environment, then poll every ~5 ms: first visible top-level window of the
    /// process, first time it answers WM_NULL, and (checked every ~40 ms) first time a PrintWindow capture
    /// shows at least `minColours` distinct colours. Does NOT kill the process; the caller does (KillTree).
    public static StartupResult MeasureStartup(string exe, string workDir, string[] envPairs, int minColours, int timeoutMs, string savePng)
    {
        var psi = new ProcessStartInfo(exe);
        psi.UseShellExecute = false;
        psi.WorkingDirectory = workDir;
        psi.CreateNoWindow = false;
        foreach (var pair in envPairs)
        {
            int eq = pair.IndexOf('=');
            if (eq > 0) psi.EnvironmentVariables[pair.Substring(0, eq)] = pair.Substring(eq + 1);
        }
        var res = new StartupResult();
        var clock = Stopwatch.StartNew();
        var proc = Process.Start(psi);
        res.Pid = proc.Id;
        try { res.StartTime = proc.StartTime; } catch { res.StartTime = DateTime.Now; }
        IntPtr hwnd = IntPtr.Zero;
        long lastCapture = -1000;
        while (clock.ElapsedMilliseconds < timeoutMs)
        {
            if (proc.HasExited) break;
            if (hwnd == IntPtr.Zero)
            {
                string t;
                hwnd = FindMainWindow(res.Pid, 300, 200, out t);
                if (hwnd != IntPtr.Zero)
                {
                    res.VisibleMs = clock.Elapsed.TotalMilliseconds;
                    res.WindowTitle = t;
                    res.WindowSize = WindowSize(hwnd);
                }
            }
            if (hwnd != IntPtr.Zero)
            {
                if (res.RespondingMs < 0 && Responding(hwnd, 100)) res.RespondingMs = clock.Elapsed.TotalMilliseconds;
                if (clock.ElapsedMilliseconds - lastCapture >= 40)
                {
                    lastCapture = clock.ElapsedMilliseconds;
                    int c = DistinctColours(hwnd, null);
                    res.Colours = c;
                    if (c >= minColours)
                    {
                        res.ContentMs = clock.Elapsed.TotalMilliseconds;
                        if (!string.IsNullOrEmpty(savePng)) DistinctColours(hwnd, savePng);
                        if (res.RespondingMs < 0 && Responding(hwnd, 200)) res.RespondingMs = clock.Elapsed.TotalMilliseconds;
                        break;
                    }
                }
            }
            Thread.Sleep(5);
        }
        return res;
    }
}
