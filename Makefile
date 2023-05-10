CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = x86_64-linux-gnu-gcc
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER

YAFFLE_SOURCES := $(shell find yaffle-server yaffle-macros -type f -name '*.rs' -or -type f -name '*.toml')
YAFFLE_PARENT_SOURCES := Cargo.lock Cargo.toml

.PHONY:all
all: image/.built

image/.built: target/x86_64-unknown-linux-gnu/release/yaffle-server image/*.*
	packer build -var deb_arch=amd64 -var container_tag="$$(git describe --tags --dirty --always)" image
	touch $@

target/x86_64-unknown-linux-gnu/release/yaffle-server: $(YAFFLE_SOURCES) $(YAFFLE_PARENT_SOURCES)
	cargo build --release --target x86_64-unknown-linux-gnu --bin yaffle-server

clean:
	cargo clean
	rm image/.built
