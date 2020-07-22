#!/bin/sh

api_base="https://api.github.com/repos"

# Function to take 2 git tags/commits and get any lines from commit messages
# that contain something that looks like a PR reference: e.g., (#1234)
sanitised_git_logs(){
  git --no-pager log --pretty=format:"%s" "$1...$2" |
  # Only find messages referencing a PR
  grep -E '\(#[0-9]+\)' |
  # Strip any asterisks
  sed 's/^* //g'
}

# Checks whether a tag on github has been verified
# repo: 'organization/repo'
# tagver: 'v1.2.3'
# Usage: check_tag $repo $tagver
check_tag () {
  repo=$1
  tagver=$2
  if [ -n "$GITHUB_RELEASE_TOKEN" ]; then
    echo '[+] Fetching tag using privileged token'
    tag_out=$(curl -H "Authorization: token $GITHUB_RELEASE_TOKEN" -s "$api_base/$repo/git/refs/tags/$tagver")
  else
    echo '[+] Fetching tag using unprivileged token'
    tag_out=$(curl -H "Authorization: token $GITHUB_PR_TOKEN" -s "$api_base/$repo/git/refs/tags/$tagver")
  fi
  tag_sha=$(echo "$tag_out" | jq -r .object.sha)
  object_url=$(echo "$tag_out" | jq -r .object.url)
  if [ "$tag_sha" = "null" ]; then
    return 2
  fi
  echo "[+] Tag object SHA: $tag_sha"
  verified_str=$(curl -H "Authorization: token $GITHUB_RELEASE_TOKEN" -s "$object_url" | jq -r .verification.verified)
  if [ "$verified_str" = "true" ]; then
    # Verified, everything is good
    return 0
  else
    # Not verified. Bad juju.
    return 1
  fi
}

# Checks whether a given PR has a given label.
# repo: 'organization/repo'
# pr_id: 12345
# label: B1-silent
# Usage: has_label $repo $pr_id $label
has_label(){
  repo="$1"
  pr_id="$2"
  label="$3"
  if [ -n "$GITHUB_RELEASE_TOKEN" ]; then
    out=$(curl -H "Authorization: token $GITHUB_RELEASE_TOKEN" -s "$api_base/$repo/pulls/$pr_id")
  else
    out=$(curl -H "Authorization: token $GITHUB_PR_TOKEN" -s "$api_base/$repo/pulls/$pr_id")
  fi
  [ -n "$(echo "$out" | tr -d '\r\n' | jq ".labels | .[] | select(.name==\"$label\")")" ]
}

github_label () {
  echo
  echo "# run github-api job for labeling it ${1}"
  curl -sS -X POST \
    -F "token=${CI_JOB_TOKEN}" \
    -F "ref=master" \
    -F "variables[LABEL]=${1}" \
    -F "variables[PRNO]=${CI_COMMIT_REF_NAME}" \
    -F "variables[PROJECT]=paritytech/polkadot" \
    "${GITLAB_API}/projects/${GITHUB_API_PROJECT}/trigger/pipeline"
}

# Formats a message into a JSON string for posting to Matrix
# message: 'any plaintext message'
# formatted_message: '<strong>optional message formatted in <em>html</em></strong>'
# Usage: structure_message $content $formatted_content (optional)
structure_message() {
  if [ -z "$2" ]; then
    body=$(jq -Rs --arg body "$1" '{"msgtype": "m.text", $body}' < /dev/null)
  else
    body=$(jq -Rs --arg body "$1" --arg formatted_body "$2" '{"msgtype": "m.text", $body, "format": "org.matrix.custom.html", $formatted_body}' < /dev/null)
  fi
  echo "$body"
}

# Post a message to a matrix room
# body: '{body: "JSON string produced by structure_message"}'
# room_id: !fsfSRjgjBWEWffws:matrix.parity.io
# access_token: see https://matrix.org/docs/guides/client-server-api/
# Usage: send_message $body (json formatted) $room_id $access_token
send_message() {
curl -XPOST -d "$1" "https://matrix.parity.io/_matrix/client/r0/rooms/$2/send/m.room.message?access_token=$3"
}

# Pretty-printing functions
boldprint () { printf "|\n| \033[1m%s\033[0m\n|\n" "${@}"; }
boldcat () { printf "|\n"; while read -r l; do printf "| \033[1m%s\033[0m\n" "${l}"; done; printf "|\n" ; }

prepare_git() {
  # Set the user name and email to make merging work
  git config --global user.name 'CI system'
  git config --global user.email '<>'
}

prepare_substrate() {
  pr_companion=$1
  boldprint "companion pr specified/detected: #${pr_companion}"

  # Clone the current Substrate master branch into ./substrate.
  # NOTE: we need to pull enough commits to be able to find a common
  # ancestor for successfully performing merges below.
  git clone --depth 20 https://github.com/paritytech/substrate.git
  previous_path=$(pwd)
  cd substrate
  SUBSTRATE_PATH=$(pwd)

  git fetch origin refs/pull/${pr_companion}/head:pr/${pr_companion}
  git checkout pr/${pr_companion}
  git merge origin/master

  cd "$previous_path"

  # Merge master into our branch before building Polkadot to make sure we don't miss
  # any commits that are required by Polkadot.
  git merge origin/master

  # Make sure we override the crates in native and wasm build
  # patching the git path as described in the link below did not test correctly
  # https://doc.rust-lang.org/cargo/reference/overriding-dependencies.html
  mkdir .cargo
  echo "paths = [ \"$SUBSTRATE_PATH\" ]" > .cargo/config

  mkdir -p target/debug/wbuild/.cargo
  cp .cargo/config target/debug/wbuild/.cargo/config
}

pull_companion_substrate() {
  github_api_polkadot_pull_url="https://api.github.com/repos/paritytech/polkadot/pulls"
  # use github api v3 in order to access the data without authentication
  github_header="Authorization: token ${GITHUB_PR_TOKEN}"


  # either it's a pull request then check for a companion otherwise use
  # substrate:master
  if expr match "${CI_COMMIT_REF_NAME}" '^[0-9]\+$' >/dev/null
  then
    boldprint "this is pull request no ${CI_COMMIT_REF_NAME}"

    pr_data_file="$(mktemp)"
    # get the last reference to a pr in substrate
    curl -sSL -H "${github_header}" -o "${pr_data_file}" \
      "${github_api_polkadot_pull_url}/${CI_COMMIT_REF_NAME}"

    pr_body="$(sed -n -r 's/^[[:space:]]+"body": (".*")[^"]+$/\1/p' "${pr_data_file}")"

    pr_companion="$(echo "${pr_body}" | sed -n -r \
        -e 's;^.*substrate companion: paritytech/substrate#([0-9]+).*$;\1;p' \
        -e 's;^.*substrate companion: https://github.com/paritytech/substrate/pull/([0-9]+).*$;\1;p' \
      | tail -n 1)"

    if [ "${pr_companion}" ]
    then
      echo Substrate path: $SUBSTRATE_PATH
      prepare_git
      prepare_substrate "$pr_companion"
    else
      boldprint "no companion branch found - building your Polkadot branch"
    fi
    rm -f "${pr_data_file}"
  else
    boldprint "this is not a pull request - building your Polkadot branch"
  fi
}
