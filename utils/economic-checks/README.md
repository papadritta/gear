A fuzzying-like framework to run checks of various invariants defined on the chain state.

Based on `cargo fuzz` intergrated with the `libfuzzer` library.

Documentation available [here](https://rust-fuzz.github.io/book/introduction.html).

<br/>

### Running fuzz targets

1. Navigate to `${GEAR_HOME_DIR}/utils/economic-checks`.

2. Run Gear node without special arguments to get a node connected to the testnet:

    ```bash
    utils/economic-checks$ cargo fuzz run --sanitizer=none simple_fuzz_target
    ```

The test is ragher lengthy, use `RUST_LOG=debug` (adjusted for respective targets) to ensure it is still running.
