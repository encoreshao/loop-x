#!/usr/bin/env bash
set -euo pipefail

# Installs the one external dependency of the GitLab issue loop, the
# `gitlab-config` skill from encore-skills (github.com/encoreshao/encore-skills).
# Run from the dashboard's Settings > Skills page; check that page afterwards
# for a live view of what is installed. Per-machine config files
# (projects.json, topics.json, loops.json, ...) are created by loopx itself the
# first time the dashboard or scheduler needs them.

if [ -t 1 ]; then
  C_BLUE=$'\033[1;34m'; C_GREEN=$'\033[1;32m'; C_RESET=$'\033[0m'
else
  C_BLUE=""; C_GREEN=""; C_RESET=""
fi

echo "${C_BLUE}==> Installing the gitlab-config skill (encore-skills)...${C_RESET}"
curl -fsSL https://raw.githubusercontent.com/encoreshao/encore-skills/main/scripts/setup.sh | bash -s -- --claude
echo "${C_GREEN}Setup done.${C_RESET} Configure ~/.gitlab/config.json and ~/.slack/config.json from the dashboard's Settings page."
