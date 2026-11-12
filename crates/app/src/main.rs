fn main() {
    if sandbox_sandboxes::sandbox::watchdog_mode() {
        return;
    }
    sandbox_workspace::run();
}
