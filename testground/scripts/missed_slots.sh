GIT_ROOT_PATH=$(git rev-parse --show-toplevel)
cd "${GIT_ROOT_PATH}/testground"

source scripts/common.sh

# If `MK1_STAGING` is set, use alternative names
DEVNET_NAME="ethereum-devnet${MK1_STAGING:+-staging}"

show_all_containers() {
  docker ps --all --format "table {{.ID}}\t{{.Names}}\t{{.Status}}"
}

grep_validator_clients_container_ids() {
  vc_clients_ids=$(kurtosis enclave inspect ${DEVNET_NAME} | grep 'vc-[0-9]-geth-lighthouse' | awk '{print $1}')
  containers --all | grep -E $(echo ${vc_clients_ids} | tr ' ' '|') | awk '{print $1}'
}

stop_validator_clients() {
  vc_clients_container_ids=$(grep_validator_clients_container_ids)
  if [ -z "${vc_clients_container_ids}" ]; then
    error "No VC clients to stop"
    return
  fi

  info "Stopping VC clients"
  docker stop ${vc_clients_container_ids}
}

start_validator_clients() {
  vc_clients_container_ids=$(grep_validator_clients_container_ids)
  if [ -z "${vc_clients_container_ids}" ]; then
    error "No VC clients to start"
    return
  fi

  info "Starting VC clients"
  docker start ${vc_clients_container_ids}
}

miss_slots() {
  info "Missing ${1} slot(s)"
  stop_validator_clients
  info "Sleeping for ${1} slot(s)"
  ((time = ${1} * 12))
  sleep ${time}
  start_validator_clients
}
