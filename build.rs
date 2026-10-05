//! Host-linker hints for MSYS2 UCRT64. Paths stay POSIX (`/ucrt64/...`)
//! so the repo has no machine-specific `C:\Users\...` prefixes.
fn main() {
    println!("cargo:rustc-link-search=native=/ucrt64/lib");
    println!("cargo:rustc-link-lib=ffi");
    println!("cargo:rustc-link-lib=stdc++");
    println!("cargo:rustc-link-lib=LLVM-22");
}
