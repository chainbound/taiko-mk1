# Release Steps

- Make sure [`rust-toolchain.toml`](./rust-toolchain.toml) and the Rust base image in [`Dockerfile`](./Dockerfile) match.
- Run `just build $TAG` to build and push the Docker image.