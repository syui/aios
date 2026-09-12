#!/bin/bash
# Default firewall: deny incoming, allow outgoing, allow SSH (22).
# Runs once at boot via aios-firewall.service.

set -e

ufw default deny incoming
ufw default allow outgoing
ufw allow 22/tcp
ufw --force enable
