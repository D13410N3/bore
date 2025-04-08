#!/bin/bash
set -e

# Build the Docker image
echo "Building Docker image for cross-compilation..."
docker build -t bore-aarch64-builder -f Dockerfile.cross .

# Create a container from the image and copy the binary
echo "Extracting the compiled binary..."
docker create --name bore-temp bore-aarch64-builder
docker cp bore-temp:/bore ./bore-aarch64-linux-musl
docker rm bore-temp

# Make the binary executable
chmod +x ./bore-aarch64-linux-musl

echo "Build complete! Binary is at: $(pwd)/bore-aarch64-linux-musl"
