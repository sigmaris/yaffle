#cloud-config
# Set root's authorized_key, to allow Packer to connect over SSH
users:
- name: root
  ssh_redirect_user: false
  ssh_authorized_keys:
  - '${root_ssh_pub_key}'
