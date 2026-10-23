// Entry point: hands the main thread to the Rust staticlib, which runs the
// winit/eframe event loop and does not return in normal operation.
extern void asteria_spike_main(void);

int main(int argc, char *argv[]) {
    asteria_spike_main();
    return 0;
}
