#!/bin/bash

# Define variables
owner="tee8z"
repo="5day4cast"
tag="v0.1.0"
server="server-x86_64-unknown-linux-gnu.tar.xz"
client_validator="clien_validator-x86_64-unknown-linux-gnu.tar.xz"

# Construct URL
server_url="https://github.com/$owner/$repo/releases/download/$tag/$server"
client_validator_url="https://github.com/$owner/$repo/releases/download/$tag/$client_validator"

# Download the binaries
curl -LJO $server_url
curl -LJO $client_validator_url

# Create a directory with the repository name
mkdir "release-$tag"

full_path=$(pwd)/"release-$tag"

# Unzip the downloaded file into the temporary directory
tar -xf $server -C $full_path
tar -xf $client_validator -C $full_path

rm $server
rm $client_validator
