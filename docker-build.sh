#!/bin/sh -eu
docker build -t yaffle-builder .
docker run --rm -it -v $(pwd):/yaffle -v $(pwd)/../Toshi:/Toshi --workdir /yaffle --user $(id -u):$(id -g) yaffle-builder ./in-container-build.sh
