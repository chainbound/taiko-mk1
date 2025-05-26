<img src="./testground/mk1_logo.png" width="128" height="128" alt="Mk1 Logo">

# `mk1` Taiko Alethia sequencer

> Mk1 is the first iteration of Chainbound's _preconfirmation service_ for rollups.

## Project structure

- `bin`: contains all executables
- `crates`: contains all the Rust crates for the Mk1 service and library
- `testground`: contains the scripts for spinning up/down the devnet environment

We use [`just`](https://github.com/casey/just) as command runner for this project.

```shell
# check out the list of available commands:
just
```

## Devnet

This repository includes a Taiko Pacaya devnet with Kurtosis and Docker.
Please check out the [testground README](./testground/README.md) for more details about this setup.

## Security

To report a vulnerability, contact <security@chainbound.io>.

## License

<sup>
MK1 Taiko Alethia Sequencer Sidecar
Copyright (C) 2025 Chainbound
</sup>

<br>

<sup>
This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.
</sup>

<br>

<sup>
This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU General Public License for more details.
</sup>

<br>

<sup>
You should have received a copy of the GNU General Public License
along with this program. If not, see https://www.gnu.org/licenses/.
</sup>
