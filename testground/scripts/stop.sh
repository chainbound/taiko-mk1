#!/bin/bash

GIT_ROOT_PATH=$(git rev-parse --show-toplevel)
cd "${GIT_ROOT_PATH}/testground"

source scripts/common.sh

# If `MK1_STAGING` is set, use alternative names
DOCKER_COMPOSE_FILE="${MK1_STAGING:+staging/}docker-compose.yml"
DEVNET_NAME="ethereum-devnet${MK1_STAGING:+-staging}"

info "Stopping testground ${DEVNET_NAME}..."
docker compose -f ${DOCKER_COMPOSE_FILE} down

# TODO: remove taiko-mono (when it's more stable)
# rm -rf taiko-mono
if ! kurtosis enclave stop ${DEVNET_NAME}; then
  error "Failed to stop Kurtosis devnet ${DEVNET_NAME}, retrying..."
  if ! kurtosis enclave stop ${DEVNET_NAME}; then
    error "Failed to stop Kurtosis devnet ${DEVNET_NAME} again, exiting"
    exit 1
  fi
fi

if ! kurtosis enclave rm ${DEVNET_NAME}; then
  error "Failed to remove Kurtosis enclave ${DEVNET_NAME}, retrying..."
  if ! kurtosis enclave rm ${DEVNET_NAME}; then
    error "Failed to remove Kurtosis enclave ${DEVNET_NAME} again, exiting"
    exit 1
  fi
fi

info "Done"
exit 0
