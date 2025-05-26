set shell := ["bash", "-cu"]
set dotenv-load := true

# display a help message about available commands
default:
    @just --list --unsorted

# start the MK1 binary for local development
dev:
    ENV_FILE=.env cargo run

# run all recipes required to pass CI workflows
ci:
    @just fmt lint test

# run unit tests
test:
    cargo nextest run --workspace -E "kind(lib) | kind(bin) | kind(proc-macro)"

# run collection of clippy lints
lint:
    RUSTFLAGS="-D warnings" cargo clippy --examples --tests --benches --all-features --locked

# format the code using the nightly rustfmt (as we use some nightly lints)
fmt:
    cargo +nightly fmt --all

# start the testground devnet setup
testground:
    (cd testground/scripts && ./start.sh --l2-multi-nodes)

# stop testground devnet setup
stop:
    (cd testground/scripts && ./stop.sh)

# build and push the docker image with the given tag for the given platforms
build tag='latest' platform='linux/amd64,linux/arm64':
    docker buildx build \
        --label "org.opencontainers.image.commit=$(git rev-parse --short HEAD)" \
        --platform {{platform}} \
        --tag ghcr.io/chainbound/mk1:{{tag}} \
        --push .
