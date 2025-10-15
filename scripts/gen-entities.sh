set -euxo pipefail

sea-orm-cli migrate up
rm -r ./database/entities/src/entities
sea-orm-cli generate entity -o ./database/entities/src/entities \
    --enum-extra-derives schemars::JsonSchema,serde::Serialize,Hash \
    --enum-extra-attributes 'serde(rename_all = "snake_case")'
