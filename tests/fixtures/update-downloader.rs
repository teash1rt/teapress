// Native curl stand-in for the Windows update test; also serves as the replacement exe.
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "--version") {
        println!("teapress test-replacement");
        return;
    }
    assert_eq!(args.last().unwrap(), "https://github.com/teash1rt/teapress/releases/latest/download/teapress-installer.ps1");
    let output = args.windows(2).find(|pair| pair[0] == "--output").unwrap();
    std::fs::copy(std::env::var_os("MOCK_INSTALLER").unwrap(), &output[1]).unwrap();
    eprintln!("downloading");
    std::process::exit(std::env::var("MOCK_DOWNLOAD_EXIT").unwrap().parse().unwrap());
}
