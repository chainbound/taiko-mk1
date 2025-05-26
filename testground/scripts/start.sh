#!/bin/bash

GIT_ROOT_PATH=$(git rev-parse --show-toplevel)
cd "${GIT_ROOT_PATH}/testground"

source scripts/common.sh

LOG_CTX="testground"

# Trap errors and print error message
trap 'error "An error occurred. Check the logs above for details"; exit 1' ERR

# If `MK1_STAGING` is set, use alternative names

DEVNET_NAME="ethereum-devnet${MK1_STAGING:+-staging}"
DOCKER_COMPOSE_FILE="${MK1_STAGING:+staging/}docker-compose.yml"
TAIKO_DRIVER_0="taiko-driver${MK1_STAGING:+-staging}-0"
TAIKO_DRIVER_1="taiko-driver${MK1_STAGING:+-staging}-1"
TAIKO_GETH_0="taiko-geth${MK1_STAGING:+-staging}-0"
TAIKO_GETH_1="taiko-geth${MK1_STAGING:+-staging}-1"

debug "Devnet name: ${DEVNET_NAME}"
debug "Using docker compose file: ${DOCKER_COMPOSE_FILE}"

TESTGROUND_DIR=$(pwd)

# Default verify flag to false
VERIFY=false

BLOCKSCOUT=false
L2_MULTI_NODES=false

# Parse command line arguments
while [[ $# -gt 0 ]]; do
  case ${1} in
  --verify)
    VERIFY=true
    shift
    ;;
  --l2-multi-nodes)
    L2_MULTI_NODES=true
    shift
    ;;
  --blockscout)
    BLOCKSCOUT=true
    shift
    ;;
  *)
    error "Unknown argument: ${1}"
    exit 1
    ;;
  esac
done

deploy() {
  if [ "${VERIFY}" = true ]; then
    cd taiko-mono/packages/protocol &&
      forge script script/layer1/based/DeployProtocolOnL1.s.sol:DeployProtocolOnL1 \
        --fork-url "${L1_EXT_HTTP}" \
        --broadcast \
        --ffi \
        -v \
        --evm-version cancun \
        --private-key "${PRIVATE_KEY}" \
        --block-gas-limit 200000000 \
        --no-rpc-rate-limit \
        --legacy \
        --verify \
        --verifier blockscout \
        --verifier-url "${BLOCKSCOUT_URL}/api/" \
        --delay 3 &&
      cd -
  else
    cd taiko-mono/packages/protocol &&
      forge script script/layer1/based/DeployProtocolOnL1.s.sol:DeployProtocolOnL1 \
        --fork-url "${L1_EXT_HTTP}" \
        --broadcast \
        --ffi \
        -v \
        --evm-version cancun \
        --private-key "${PRIVATE_KEY}" \
        --block-gas-limit 200000000 \
        --no-rpc-rate-limit \
        --legacy &&
      cd -
  fi
}

# Check if required commands are available
check_cmd kurtosis
check_cmd docker
check_cmd git
check_cmd pnpm
check_cmd jq
check_cmd forge
check_cmd cast

KURTOSIS_EXISTS=false

[[ "${L2_MULTI_NODES}" = true ]] && info "Running with 2 L2 nodes"

if ! kurtosis enclave inspect ${DEVNET_NAME} 2>&1 >/dev/null; then
  info "Starting Kurtosis..."
  if [[ "${VERIFY}" = true || "${BLOCKSCOUT}" = true ]]; then
    info "Running with blockscout"
    kurtosis run --enclave ${DEVNET_NAME} github.com/ethpandaops/ethereum-package --args-file network_params_blockscout.yml --verbosity OUTPUT_ONLY
  else
    kurtosis run --enclave ${DEVNET_NAME} github.com/ethpandaops/ethereum-package --args-file network_params.yml --verbosity OUTPUT_ONLY
  fi
  info "Kurtosis started"
else
  info "Kurtosis already running"
  KURTOSIS_EXISTS=true
fi

DORA_URL=http://$(kurtosis service inspect ${DEVNET_NAME} dora | grep -o "127.0.0.1:[0-9]*")
if [[ "${VERIFY}" = true || "${BLOCKSCOUT}" = true ]]; then
  BLOCKSCOUT_URL=http://$(kurtosis service inspect ${DEVNET_NAME} blockscout | grep -o "127.0.0.1:[0-9]*")
  BLOCKSCOUT_FRONTEND=http://$(kurtosis service inspect ${DEVNET_NAME} blockscout-frontend | grep -o "127.0.0.1:[0-9]*")
fi

info "Cloning Taiko repository..."
(git clone -q https://github.com/taikoxyz/taiko-mono.git 2>/dev/null || info "Taiko repository already cloned, skipping") &&
  cd taiko-mono &&
  git fetch && git checkout preconf_configs_4 &&
  pnpm install && cd - &&
  # Copy the patched DevnetInbox.sol to the taiko-mono repository
  cp DevnetInbox.testground.sol taiko-mono/packages/protocol/contracts/layer1/devnet/DevnetInbox.sol

info "Dora: ${DORA_URL}"
[[ "${VERIFY}" = true || "${BLOCKSCOUT}" = true ]] && info "Blockscout: ${BLOCKSCOUT_FRONTEND}"

EL_SERVICE_UUID=$(kurtosis service inspect ${DEVNET_NAME} "el-1-geth-lighthouse" | grep -o "UUID: [0-9a-f-]*" | cut -d' ' -f2)
CL_SERVICE_UUID=$(kurtosis service inspect ${DEVNET_NAME} "cl-1-lighthouse-geth" | grep -o "UUID: [0-9a-f-]*" | cut -d' ' -f2)

EL_HOST=$(docker ps --filter name=${EL_SERVICE_UUID} --format json | jq -r .Names)
export L1_HTTP="http://${EL_HOST}:8545"

export L1_WS="ws://${EL_HOST}:8546"

CL_HOST=$(docker ps --filter name=${CL_SERVICE_UUID} --format json | jq -r .Names)
export L1_BEACON="http://${CL_HOST}:4000"

# get the execution rpc url
L1_RPC_PORT=$(kurtosis service inspect ${DEVNET_NAME} ${EL_SERVICE_UUID} | grep "[[:space:]]rpc: " | grep -o "127.0.0.1:[0-9]*" | cut -d':' -f2)
export L1_EXT_HTTP="http://127.0.0.1:${L1_RPC_PORT}"

# get the beacon rpc url
L1_CL_RPC=$(kurtosis service inspect ${DEVNET_NAME} ${CL_SERVICE_UUID} | grep '^ *http: 4000/tcp' | grep -o 'http://127\.0\.0\.1:[0-9]*')

if [ "${KURTOSIS_EXISTS}" = false ]; then
  info "Waiting for Kurtosis to be ready..."
  # Wait for L1 node to be ready to receive transactions
  while true; do
    FIRST_BLOCK=$(curl -s -X POST -H 'content-type: application/json' \
      -d '{"id": null, "jsonrpc": "2.0", "method": "eth_getBlockByNumber", "params": ["0x1", false]}' \
      ${L1_EXT_HTTP} | jq -r .result)

    if [ "${FIRST_BLOCK}" != "null" ]; then
      info "L1 node is ready"
      break
    fi

    info "Waiting for first block to be proposed..."
    sleep 5
  done

  # sleep 90
fi

if [ "${L2_MULTI_NODES}" = true ]; then
  info "Starting 2 instances of taiko-geth"
  docker compose -f ${DOCKER_COMPOSE_FILE} up ${TAIKO_GETH_0} ${TAIKO_GETH_1} -d
else
  info "Starting 1 instance of taiko-geth"
  docker compose -f ${DOCKER_COMPOSE_FILE} up ${TAIKO_GETH_0} -d
fi

sleep 5

# We get the L2 genesis hash from the Taiko geth node
info "Getting L2 genesis hash"
export L2_GENESIS_HASH=$(
  curl \
    --silent \
    -X POST \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":0,"method":"eth_getBlockByNumber","params":["0x0", false]}' \
    http://localhost:8544 | jq -r .result.hash
)

# Set Taiko environment variables. These will be read in scripts and docker-compose.yml
export PRIVATE_KEY=0xbcdf20249abf0ed6d944c0288fad489e33f66b3960d9e6229c1cd214ed3bbe31
export PRIVATE_KEY=${PRIVATE_KEY}
export OLD_FORK_TAIKO_INBOX=0x0000000000000000000000000000000000000001
export TAIKO_TOKEN=0x0000000000000000000000000000000000000000
export TAIKO_ANCHOR_ADDRESS=0x1000777700000000000000000000000000000001
export L2_SIGNAL_SERVICE=0x1000777700000000000000000000000000000007
export CONTRACT_OWNER=0x8943545177806ED17B9F23F0a21ee5948eCaa776
export PROVER_SET_ADMIN=0x8943545177806ED17B9F23F0a21ee5948eCaa776
export SHARED_RESOLVER=0x0000000000000000000000000000000000000000
export PAUSE_BRIDGE=true
export FOUNDRY_PROFILE="layer1"
export DEPLOY_PRECONF_CONTRACTS=true
export PRECONF_INBOX=false
export SECURITY_COUNCIL=0x8943545177806ED17B9F23F0a21ee5948eCaa776
export TAIKO_TOKEN_PREMINT_RECIPIENT=0x8943545177806ED17B9F23F0a21ee5948eCaa776
export TAIKO_TOKEN_NAME="Taiko Token"
export TAIKO_TOKEN_SYMBOL=TAIKO
export INCLUSION_WINDOW=24
export INCLUSION_FEE_IN_GWEI=100
export TAIKO_ANCHOR=0x1670100000000000000000000000000000010001
export TAIKO_INBOX=0x23B4c59C3B67A512563D8650d2C78Ec3861c4648
export PRECONF_ROUTER=0xdd66a0C681fBC69D00D929056d55899967ABcd0E
export PRECONF_WHITELIST=0xF02a43985ab5011af94F6d4dAd454C5E305A3e42
export PRECONFIRMATION_WHITELIST=${PRECONF_WHITELIST}
export DUMMY_VERIFIERS=true

if [ "${KURTOSIS_EXISTS}" = false ]; then
  info "Hardcoding genesis timestamp in LibPreconfConstants"

  genesis_time="$(curl -s ${L1_CL_RPC}/eth/v1/beacon/genesis | jq .data.genesis_time | tr -d '"')"

  # Replace line 26 of LibPreconfConstants with "return ${genesis_time};"
  #
  # This is ugly af but needed to add a reliable randomness for the preconf whitelist in the devnet
  SOL_FILE="taiko-mono/packages/protocol/contracts/layer1/preconf/libs/LibPreconfConstants.sol"
  if [[ "${OSTYPE}" == "darwin"* ]]; then
    sed -i '' "26s/.*/        return ${genesis_time};/" "${SOL_FILE}"
  else
    sed -i "26s/.*/        return ${genesis_time};/" "${SOL_FILE}"
  fi

  info "✅ Line 26 replaced with 'return ${genesis_time};' in LibPreconfConstants.sol"

  info "Deploying Taiko contracts on L1"

  # Deploy Taiko contracts on L1
  deploy

  export TAIKO_TOKEN=0x422A3492e218383753D8006C7Bfa97815B44373F

  cd ${TESTGROUND_DIR}

  info "Whitelisting operator: ${CONTRACT_OWNER}"
  cast send --rpc-url ${L1_EXT_HTTP} --private-key ${PRIVATE_KEY} ${PRECONF_WHITELIST} "addOperator(address)" ${CONTRACT_OWNER} --legacy

  info "Approving TaikoInbox to transfer \${TAIKO_TOKEN} for deposit bond"
  cast send --rpc-url ${L1_EXT_HTTP} --private-key ${PRIVATE_KEY} ${TAIKO_TOKEN} "approve(address,uint256)" ${TAIKO_INBOX} 100000000000000000000000000 --legacy

  info "Contracts deployed"
else
  info "Contracts already deployed, skipping deployment"
fi

info "Starting Taiko driver"
if [ "${L2_MULTI_NODES}" = true ]; then
  docker compose -f ${DOCKER_COMPOSE_FILE} up ${TAIKO_DRIVER_0} ${TAIKO_DRIVER_1} -d
else
  docker compose -f ${DOCKER_COMPOSE_FILE} up ${TAIKO_DRIVER_0} -d
fi

info "L2 genesis hash: ${L2_GENESIS_HASH}"

PORT=$(docker inspect --format='{{(index (index .NetworkSettings.Ports "8080/tcp") 0).HostPort}}' ${TAIKO_DRIVER_0})
if [ -z "${PORT}" ]; then
  error "Could not get port mapping for ${TAIKO_DRIVER_0}:8080"
  exit 1
fi

PRECONF_RPC="http://127.0.0.1:${PORT}"
EL_RPC="http://$(kurtosis port print ${DEVNET_NAME} el-1-geth-lighthouse rpc)"
EL_WS="ws://$(kurtosis port print ${DEVNET_NAME} el-1-geth-lighthouse ws)"
CL_RPC="$(kurtosis port print ${DEVNET_NAME} cl-1-lighthouse-geth http)"

info "======================================================================"
info "TESTGROUND RUNNING"
info "Run ./stop.sh to stop testground"
info "Dora: ${DORA_URL}"
[[ "${VERIFY}" = true || "${BLOCKSCOUT}" = true ]] && info "Blockscout: ${BLOCKSCOUT_FRONTEND}"
info "Sequencer operator: ${CONTRACT_OWNER}"
info "Preconfirmation RPC: ${PRECONF_RPC}"
info "======================================================================"
info "MAKE SURE TO COPY THE FOLLOWING VALUES TO YOUR .env FILE:"
info "MK1_L1_EXECUTION_URL=${EL_RPC}"
info "MK1_L1_EXECUTION_WS_URL=${EL_WS}"
info "MK1_L1_CONSENSUS_URL=${CL_RPC}"
info "======================================================================"
