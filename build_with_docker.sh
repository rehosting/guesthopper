#!/bin/bash

docker run -it --rm -v $PWD:/app -w /app rehosting/embedded-toolchains_rust:latest /app/package.sh
