# 5day4cast
Holds the MVP of fantasy weather

### To compile add duckdb lib:
```
wget https://github.com/duckdb/duckdb/releases/download/v1.0.0/libduckdb-linux-amd64.zip
mkdir duckdb_lib
unzip libduckdb-linux-amd64.zip -d duckdb_lib
sudo cp duckdb_lib/lib*.so* /usr/local/lib/
sudo ldconfig
rm libduckdb-linux-amd64.zip
```

### How to run
- at the root of the repo run `cargo run --bin server -- --config ./Settings.toml`
