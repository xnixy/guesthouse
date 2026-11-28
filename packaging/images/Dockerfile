# The distribution images the packaging scripts run in, each named by tag and digest. Nothing builds
# this file: packaging/images/image-of.sh reads the image of a name from it, and Dependabot proposes
# new digests for it.
FROM docker.io/library/debian:trixie@sha256:3cda3ca472ce45507a5109c6a3907093e24d9ae67d55954c9a07a45665dafba2 AS debian
FROM registry.fedoraproject.org/fedora:44@sha256:c1e457a3c398b0d7b12d060ad86ad52bab714a17faecc907deb1262fb2b72d98 AS fedora
FROM docker.io/library/archlinux:base@sha256:375a401bb3c6376b37f40b6dfa25067db37ec78d0b7af67ae6315f43a0184a9d AS archlinux
