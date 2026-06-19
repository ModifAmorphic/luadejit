//! luadejit CLI: decompile LuaJIT bytecode files.
//!
//! Stage 0: reads a file, calls `luadejit_core::decompile`, prints
//! result or error. Real CLI features (output directories, chunkname
//! handling, verbose mode, etc.) come in later stages.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        let prog = args.first().map(String::as_str).unwrap_or("luadejit");
        eprintln!("Usage: {} <input.bc>", prog);
        return ExitCode::from(1);
    }

    let input_path = &args[1];
    let bytes = match std::fs::read(input_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error reading {}: {}", input_path, e);
            return ExitCode::from(1);
        }
    };

    match luadejit_core::decompile(&bytes) {
        Ok(source) => {
            print!("{}", source);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Decompile error: {}", e);
            ExitCode::from(1)
        }
    }
}
