#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    rustquit_fuzz::check_config(data);
});
