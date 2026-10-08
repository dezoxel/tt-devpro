#![deny(warnings)]
//! The `tt-devpro` binary: the tokio entry point around [`tt_devpro::run`].

#[tokio::main]
async fn main() {
    tt_devpro::run().await;
}
