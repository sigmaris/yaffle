debian_img_datestamp = "20230411"
debian_img_timestamp = "1347"
debian_img_sha512s = {
  "amd64" = "a00082a078811150d4ad61ac273717d7ede2d544255bc120270d074373a516c5d631d470a99dac5f1b0ed88f643047b983a08bb1ba4a01942614bf8023360861"
  "arm64" = "b045be6902aea49c7cb961550ba5361068a685bd7202c8d6d4b6e0f83deb5e3a3ca22f1499732ff7d34a7bf16f35a1d0bc991e975ff11b394197136273798bfd"
}
efi_firmware_paths = {
  "amd64" = {
    "code" = "/usr/local/share/qemu/edk2-x86_64-code.fd"
    "vars" = "/usr/local/share/qemu/edk2-i386-vars.fd"
  }
  "arm64" = {
    "code" = "/usr/local/share/qemu/edk2-aarch64-code.fd"
    "vars" = "/usr/local/share/qemu/edk2-arm-vars.fd"
  }
}
