#!/bin/bash

# Reference: https://en.wikipedia.org/wiki/ANSI_escape_code#colors
#
# NOTE: the -e flag in echo enables interpretation of backslash escapes like `\n`, `\t`, etc.

error() {
  # Print timestamp and message with ERROR in red
  echo -e "${LOG_CTX:+\033[1m(${LOG_CTX})\033[0m }\033[31mERROR\033[0m [$(date '+%H:%M:%S')] ${1}"
}

warn() {
  # Print timestamp and message with WARN in yellow
  echo -e "${LOG_CTX:+\033[1m(${LOG_CTX})\033[0m }\033[33mWARN\033[0m [$(date '+%H:%M:%S')] ${1}"
}

info() {
  # Print timestamp and message with INFO in green
  echo -e "${LOG_CTX:+\033[1m(${LOG_CTX})\033[0m }\033[32mINFO\033[0m [$(date '+%H:%M:%S')] ${1}"
}

debug() {
  # Print timestamp and message with DEBUG in blue
  echo -e "${LOG_CTX:+\033[1m(${LOG_CTX})\033[0m }\033[34mDEBUG\033[0m [$(date '+%H:%M:%S')] ${1}"
}

trace() {
  # Print timestamp and message with TRACE in pink
  echo -e "${LOG_CTX:+\033[1m(${LOG_CTX})\033[0m }\033[35mTRACE\033[0m [$(date '+%H:%M:%S')] ${1}"
}

check_cmd() {
  if ! command -v ${1} &>/dev/null; then
    error "${1}: command could not be found"
    exit 1
  fi
}
