packer {
  required_plugins {
    docker = {
      version = ">= 0.0.7"
      source  = "github.com/hashicorp/docker"
    }
    qemu = {
      version = ">= 1.0.1"
      source  = "github.com/hashicorp/qemu"
    }
    sshkey = {
      version = ">= 1.0.1"
      source  = "github.com/ivoronin/sshkey"
    }
    external = {
      version = ">= 0.0.2"
      source  = "github.com/joomcode/external"
    }
  }
}

variable "deb_arch" {
  type = string
  validation {
    condition     = var.deb_arch == "amd64" || var.deb_arch == "arm64"
    error_message = "The deb_arch value must be one of \"amd64\" or \"arm64\"."
  }
}

variable "debian_img_datestamp" {
  type = string
  validation {
    condition     = can(regex("20\\d{6}", var.debian_img_datestamp))
    error_message = "The debian_img_datestamp must be in YYYYMMDD format."
  }
}

variable "debian_img_timestamp" {
  type = string
  validation {
    condition     = can(regex("\\d{4}", var.debian_img_timestamp))
    error_message = "The debian_img_timestamp must be in HHMM format."
  }
}

variable "debian_img_sha512s" {
  type = map(string)
}

variable "efi_firmware_paths" {
  type = map(map(string))
}

variable "container_tag" {
  type = string
}

data "external-raw" "host_uname_kernel" {
  program = ["uname", "-s"]
}

data "external-raw" "host_uname_machine" {
  program = ["uname", "-m"]
}

source "docker" "yaffle" {
  image  = "debian:bookworm-${var.debian_img_datestamp}"
  commit = true
  changes = [
    "USER yaffle",
    "ENV LEPTOS_OUTPUT_NAME yaffle",
    "ENV LEPTOS_SITE_ROOT /opt/yaffle/site",
    "ENV LEPTOS_SITE_ADDR 0.0.0.0:8088",
    "ENV RUST_LOG info",
    "ENTRYPOINT cd /opt/yaffle; exec /opt/yaffle/yaffle-server",
    "EXPOSE 8088",
  ]
}

data "sshkey" "install" {
  type = "ed25519"
}

locals {
  host_uname_machine = trimspace(data.external-raw.host_uname_machine.result)
  host_uname_kernel  = trimspace(data.external-raw.host_uname_kernel.result)
  host_arch = lookup({
    "aarch64" = "arm64"
    "arm64"   = "arm64"
    "x86_64"  = "amd64"
  }, local.host_uname_machine, "unknown")
  cross_compile = (local.host_arch != var.deb_arch)
  qemu_accel    = local.cross_compile ? "none" : (local.host_uname_kernel == "Darwin" ? "hvf" : "kvm")
  qemu_cpu      = local.cross_compile ? "max" : "host"
  qemu_binary   = var.deb_arch == "amd64" ? "qemu-system-x86_64" : "qemu-system-aarch64"
  qemu_machine  = var.deb_arch == "amd64" ? "q35" : "virt"
  rust_target = lookup({
    "arm64" = "aarch64-unknown-linux-gnu"
    "amd64" = "x86_64-unknown-linux-gnu"
  }, var.deb_arch, "unknown")
}

source "qemu" "bookworm" {
  accelerator          = local.qemu_accel
  iso_url              = "https://cloud.debian.org/images/cloud/bookworm/daily/${var.debian_img_datestamp}-${var.debian_img_timestamp}/debian-12-generic-${var.deb_arch}-daily-${var.debian_img_datestamp}-${var.debian_img_timestamp}.qcow2"
  iso_checksum         = "sha512:${var.debian_img_sha512s[var.deb_arch]}"
  iso_target_extension = "qcow2"
  disk_image           = true
  format               = "raw"
  headless             = false
  boot_wait            = "3s"
  use_default_display  = true
  machine_type         = local.qemu_machine
  cpu_model            = local.qemu_cpu
  qemu_binary          = local.qemu_binary
  // qemuargs = [
  //   ["-vga", "virtio"] # if vga is not virtio, output is garbled for some reason
  // ]
  efi_firmware_code = var.efi_firmware_paths[var.deb_arch]["code"]
  efi_firmware_vars = var.efi_firmware_paths[var.deb_arch]["vars"]
  cd_content = {
    "meta-data"   = "instance-id: packer-qemu\n"
    "vendor-data" = ""
    "user-data" = templatefile("${path.root}/user-data.yaml.tpl", {
      root_ssh_pub_key = data.sshkey.install.public_key
    })
  }
  cd_label             = "cidata"
  ssh_username         = "root"
  ssh_private_key_file = data.sshkey.install.private_key_path
}

build {
  name = "yaffle-server"
  sources = [
    "source.docker.yaffle"
  ]
  provisioner "shell" {
    inline = [
      "useradd --system --home-dir /opt/yaffle --create-home --shell /sbin/nologin --user-group --comment 'Yaffle Server' yaffle",
    ]
  }
  provisioner "file" {
    source      = "${path.root}/../target/server/${local.rust_target}/release/yaffle-server"
    destination = "/opt/yaffle/yaffle-server"
  }
  provisioner "file" {
    source      = "${path.root}/../target/site"
    destination = "/opt/yaffle"
  }
  post-processor "docker-tag" {
    repository = "ghcr.io/sigmaris/yaffle"
    tags = ["latest", var.container_tag]
  }
}
