CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = x86_64-linux-gnu-gcc
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER
LEPTOS_BIN_TARGET_TRIPLE = x86_64-unknown-linux-gnu
export LEPTOS_BIN_TARGET_TRIPLE
PKG_NAME = yaffle

YAFFLE_SOURCES := $(shell find yaffle-server yaffle-macros app frontend -type f -name '*.rs' -or -type f -name '*.toml')
YAFFLE_PARENT_SOURCES := Cargo.lock Cargo.toml
STATIC_FILES := target/site/pkg/$(PKG_NAME).css target/site/pkg/$(PKG_NAME).js target/site/pkg/$(PKG_NAME).wasm
COMPRESSED_STATIC_FILES := $(addsuffix .br,$(STATIC_FILES)) $(addsuffix .gz,$(STATIC_FILES))

.PHONY:all
all: image/.built

image/.built: target/server/$(LEPTOS_BIN_TARGET_TRIPLE)/release/yaffle-server $(STATIC_FILES) $(COMPRESSED_STATIC_FILES) image/*.*
	packer build -var deb_arch=amd64 -var container_tag="$$(git describe --tags --dirty --always)" image
	touch $@

target/server/$(LEPTOS_BIN_TARGET_TRIPLE)/release/yaffle-server $(STATIC_FILES): $(YAFFLE_SOURCES) $(YAFFLE_PARENT_SOURCES)
	cargo leptos build --release

%.br: %
	brotli -9 -o '$@' '$<'

%.gz: %
	gzip -9 -c '$<' > '$@'

clean:
	cargo clean
	rm image/.built
