#![feature(rustc_private)]

fn main() {
  env_logger::init();
  rustc_plugin::cli_main(print_all_items::PrintAllItemsPlugin);
}
