// 子系统说明：构建产物使用 **console** 子系统（刻意不再用 windows 子系统）。
// 原因：Agent/脚本接入 CLI 的标配方式是 shell 管道捕获（如 PowerShell 的
// `$o = & sightreel help --json`）。windows 子系统程序的 stdout 不被
// PowerShell 接入管道（实测 line_count=0），只有 -RedirectStandardOutput
// 到文件才能拿到——等于逼每个接入方踩坑。console 子系统下标准句柄正常
// 继承，管道/文件/控制台三种捕获方式全部可用。
// GUI 侧的代价是启动时系统会分配一个控制台窗口，由 main 开头的 FreeConsole
// 立即关闭（尚未绘制即释放，肉眼基本无感），GUI 运行期不残留任何控制台。

fn main() {
    // 首参是子命令（screenshot/record/help）→ CLI 无头模式，不建任何窗口；
    // 否则照常启动 GUI（托盘 + 快捷键）。
    let args: Vec<String> = std::env::args().skip(1).collect();
    if sightreel_lib::cli::wants_cli(&args) {
        std::process::exit(sightreel_lib::cli::run(&args));
    }
    // GUI 路径：释放启动时分配的控制台（见文件头说明）
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Console::FreeConsole;
        let _ = FreeConsole();
    }
    sightreel_lib::run()
}
