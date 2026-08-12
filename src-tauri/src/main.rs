#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    docker_pull_tar_lib::run();
}
