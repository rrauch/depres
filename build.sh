#!/bin/bash
rm -f out/amd64/depres
docker build \
  --platform linux/amd64 \
  -o out/amd64/ \
  ./ -t depres:amd64

rm -f out/arm64/depres
docker build \
  --platform linux/arm64 \
  -o out/arm64/ \
  ./ -t depres:arm64
