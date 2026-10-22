fn main() {
    let code = match ghost::cli::run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("ghost: {err:#}");
            1
        }
    };
    std::process::exit(code);
}
