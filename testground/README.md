# `testground`

#### Requirements

- [just](https://github.com/casey/just)
- [kurtosis](https://docs.kurtosis.com/install/)
- [docker](https://www.docker.com/get-started/)
- [pnpm](https://pnpm.io/installation)
- [jq](https://jqlang.org/download/)
- [foundry](https://book.getfoundry.sh/getting-started/installation)

#### Usage

```shell
# start the devnet environment:
just testground

# Stop the devnet environment:
just stop
```

Once the devnet is up and running (this will take a few minutes) you will see the L1 EL and CL addresses output to the console. These are required for Mk1 to work, so you should copy their values and paste them in your `.env` file.

First, copy the .env.example file to a new .env:

```shell
cp .env.example .env
```

Then, make sure to fill in the "node connections" section with the socket addresses output in the console.
All other variables can be left to their default value.

Once completing this step, you can finally start the `Mk1` binary:

```shell
just dev
```

If everything was done correctly, you will see L2 blocks being created by `Mk1` in the console logs.

#### Under the hood

1. Kurtosis spins up a local L1 devnet with some pre-funded accounts:

```bash
kurtosis run --enclave ethereum-devnet github.com/ethpandaops/ethereum-package --args-file network_params.yml
```

> [!NOTE]
> The provided [jwt.hex](./jwt.hex) file contains the JWT that is used in Kurtosis as well.

2. Taiko L1 contracts are deployed using Forge scripts. **With `--verify`, a Blockscout instance is deployed and all contracts are verified on it (this may take a while)**.

3. Taiko L2 devnet is deployed using `docker compose`.

4. An L2 operator is whitelisted for proposing blocks (address: `0x8943545177806ED17B9F23F0a21ee5948eCaa776`).

5. Taiko L2 contracts are deployed using Forge scripts.

#### Hot-reloading the devnet

You can re-run `just testground` to update the testground.

If Kurtosis is already running, it won't restart it.
It will only restart the docker containers if the image / configuration has changed.
