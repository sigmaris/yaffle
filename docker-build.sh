#!/bin/sh -eu
docker run --rm -it -v $(pwd):/yaffle -v $(pwd)/../Toshi:/Toshi --workdir /yaffle --user $(id -u):$(id -g) rust:1-buster ./in-container-build.sh