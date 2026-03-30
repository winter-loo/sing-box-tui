#!/usr/bin/env bash

set -euo pipefail

app_name="sing-box-tui"
target="${1:?usage: scripts/package-release.sh <target-triple> <version> [binary-name]}"
version="${2:?usage: scripts/package-release.sh <target-triple> <version> [binary-name]}"
binary_name="${3:-$app_name}"
binary_path="target/${target}/release/${binary_name}"
staging_dir="dist/${app_name}-${version}-${target}"
archive_path="dist/${app_name}-${version}-${target}.tar.gz"

if [[ ! -f "${binary_path}" ]]; then
  echo "expected release binary at ${binary_path}" >&2
  exit 1
fi

rm -rf "${staging_dir}"
mkdir -p "${staging_dir}"

cp "${binary_path}" "${staging_dir}/${app_name}"

for extra_file in README.md LICENSE LICENSE.md; do
  if [[ -f "${extra_file}" ]]; then
    cp "${extra_file}" "${staging_dir}/"
  fi
done

tar -C dist -czf "${archive_path}" "$(basename "${staging_dir}")"
rm -rf "${staging_dir}"

echo "created ${archive_path}"
