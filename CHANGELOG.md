# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.1.0 - 2026-10-27

### 💫 Features

- **acp** Add deterministic test-agent scenarios ([#97](https://github.com/noirbizarre/bee/issues/97)) - ([1be7cd9](https://github.com/noirbizarre/bee/commit/1be7cd9dbb3e80fa3d2ce3ad5ef3499f3294816e))
- **acp** Publish durable tool lifecycles and terminals ([#94](https://github.com/noirbizarre/bee/issues/94)) - ([ead38c5](https://github.com/noirbizarre/bee/commit/ead38c554288d84eda5ba160cc24285cdf13d0ef))
- **acp** Emit durable execution plan updates ([#60](https://github.com/noirbizarre/bee/issues/60)) - ([7cccee0](https://github.com/noirbizarre/bee/commit/7cccee06a715b0711d3b522dfecd3bf0e1fe73c1))
- **acp** Implement permission and elicitation interactions ([#48](https://github.com/noirbizarre/bee/issues/48)) - ([40fcb30](https://github.com/noirbizarre/bee/commit/40fcb304dcd778852f875600959be421c2edf20e))
- **acp** Cancel running prompts and map protocol errors - ([d752314](https://github.com/noirbizarre/bee/commit/d75231449e454944565ec6bed590ef9e95b736df))
- **acp** Implement the session and prompt lifecycle - ([3e72da2](https://github.com/noirbizarre/bee/commit/3e72da205742bf4ecb8da6af6b4f877b83faa862))
- **acp** Serve ACPv2 over stdio - ([55a864b](https://github.com/noirbizarre/bee/commit/55a864bfb20dcfcfb6a1ec867fb5d706298968b5))
- **acp** Validate MCP-over-ACP tool invocation - ([baa6791](https://github.com/noirbizarre/bee/commit/baa6791fb6803c671ac0275437242c20d7b8c3d5))
- **acp** Validate the ACPv2 Rust SDK integration - ([87a2a4b](https://github.com/noirbizarre/bee/commit/87a2a4b3fdc1624097ba6cefb593c0416ad4647f))
- **agent** Complete bounded native tool execution ([#41](https://github.com/noirbizarre/bee/issues/41)) - ([0ca857e](https://github.com/noirbizarre/bee/commit/0ca857e0b17920a371a524b18c2c882daddf61f7))
- **auth** Add ChatGPT OAuth authentication ([#61](https://github.com/noirbizarre/bee/issues/61)) - ([7897358](https://github.com/noirbizarre/bee/commit/78973586bf12a3dfa420eb46f34cba530fbbb420))
- **auth** Authenticate providers with env, stored and browser credentials - ([b5f0786](https://github.com/noirbizarre/bee/commit/b5f0786bf328e9d0783af336c13b1a49740d80fd))
- **config** Layer configuration from XDG, BEE_CONFIG and the project - ([f0f6e75](https://github.com/noirbizarre/bee/commit/f0f6e75fb0626f76db7bb369da5250751932036a))
- **context** Manage conversation history and token limits ([#43](https://github.com/noirbizarre/bee/issues/43)) - ([a7270a6](https://github.com/noirbizarre/bee/commit/a7270a637579bf7734f31dd1f3cdae0e5649dc30))
- **history** Persist sessions and replay ACP conversations ([#57](https://github.com/noirbizarre/bee/issues/57)) - ([c20d85f](https://github.com/noirbizarre/bee/commit/c20d85f6aa4c5d545da2a34a9526a85bbccd0c85))
- **instructions** Support AGENTS.md and standard Agent Skills ([#45](https://github.com/noirbizarre/bee/issues/45)) - ([a71245d](https://github.com/noirbizarre/bee/commit/a71245d908cb77c7a17f73bc2a907e5cc7635694))
- **llm** Add provider-native prompt caching ([#91](https://github.com/noirbizarre/bee/issues/91)) - ([77f6ce5](https://github.com/noirbizarre/bee/commit/77f6ce563893b7071a2d7eb882820b4d7c3a1fb3))
- **llm** Implement Anthropic Messages inference - ([a310d9d](https://github.com/noirbizarre/bee/commit/a310d9dda6fce323c1005fb54cf92dc6b9fc3c03))
- **llm** Implement OpenAI Responses inference - ([22d79c8](https://github.com/noirbizarre/bee/commit/22d79c8b63e25b7237011e04671e983f0f652e19))
- **llm** Implement Chat Completions inference - ([94f8ec9](https://github.com/noirbizarre/bee/commit/94f8ec924ad50a61eab09030bf7b89e12166d3f4))
- **mcp** Integrate client-hosted tools through ACPv2 ([#44](https://github.com/noirbizarre/bee/issues/44)) - ([a5345e1](https://github.com/noirbizarre/bee/commit/a5345e16591d934f03c027b40c84a8741d25f1bc))
- **models** Discover models for authenticated connections ([#67](https://github.com/noirbizarre/bee/issues/67)) - ([6d78529](https://github.com/noirbizarre/bee/commit/6d78529bbe13fe20c21dc2698b5e70f7a34bae6b))
- **models** Add optional catalog discovery and metadata ([#51](https://github.com/noirbizarre/bee/issues/51)) - ([cb65ac7](https://github.com/noirbizarre/bee/commit/cb65ac76c9113f751245e031ee5b7a54a8a0a07b))
- **permissions** Expand client-owned ACP permission integration ([#96](https://github.com/noirbizarre/bee/issues/96)) - ([ca0d020](https://github.com/noirbizarre/bee/commit/ca0d0201591f42f43e3e6be4e51279807f7badd6))
- **preset** Describe provider presets and login flows as data - ([bbd9887](https://github.com/noirbizarre/bee/commit/bbd98870bcbf1efbd48bd5141f6e5afa7d65234f))
- **profiles** Add named profiles and runtime selection ([#66](https://github.com/noirbizarre/bee/issues/66)) - ([2fcdb91](https://github.com/noirbizarre/bee/commit/2fcdb9159637dcb2d7881221317d4bb7f016434a))
- **provider** Add GitHub Copilot support ([#93](https://github.com/noirbizarre/bee/issues/93)) - ([4b491a4](https://github.com/noirbizarre/bee/commit/4b491a4a8c3501d69db0b769c46dac42cc96acf2))
- **provider** Select inference protocols per model ([#38](https://github.com/noirbizarre/bee/issues/38)) - ([754a5ec](https://github.com/noirbizarre/bee/commit/754a5ec945138b1d900740315f27a6fd1c1c444f))
- **provider** Add Responses protocol and well-known provider presets - ([efe66fc](https://github.com/noirbizarre/bee/commit/efe66fcc07afbc0066fabaca9b2d4e89ef9d5a47))
- **provider**  🚨 **breaking** Let a provider speak several protocols - ([e2ca05d](https://github.com/noirbizarre/bee/commit/e2ca05d2b3f1a620625d007d2eca9f85c6c8cf4c))
- **provider** Configure provider profiles and select them per session - ([c7d642e](https://github.com/noirbizarre/bee/commit/c7d642ee948c05b4ff8ec202d712a3670ac675ec))
- **selection** Persist per-profile last-used models ([#68](https://github.com/noirbizarre/bee/issues/68)) - ([b8c3eeb](https://github.com/noirbizarre/bee/commit/b8c3eeb509a7d27312f4340098634526cae237f7))
- **tools** Add embedded native content search ([#49](https://github.com/noirbizarre/bee/issues/49)) - ([303ff9c](https://github.com/noirbizarre/bee/commit/303ff9ccbb8fdde62411b6326c48c968d5ec834f))
- **tools** Add native Bash command execution ([#42](https://github.com/noirbizarre/bee/issues/42)) - ([94510aa](https://github.com/noirbizarre/bee/commit/94510aabf190b91e22a878fa260422e72cc97721))
- **tools** Add native write and edit tools ([#40](https://github.com/noirbizarre/bee/issues/40)) - ([aa89fad](https://github.com/noirbizarre/bee/commit/aa89fad7f8c8764aeb3e9d0a102d563cb7be2f4a))
- **tools** Add native file reading and discovery ([#39](https://github.com/noirbizarre/bee/issues/39)) - ([a3b825b](https://github.com/noirbizarre/bee/commit/a3b825bee81483faaad449700225c79c6827a6b8))
- **tools** Define native tool execution interface ([#37](https://github.com/noirbizarre/bee/issues/37)) - ([e907520](https://github.com/noirbizarre/bee/commit/e907520d996b6066d75384a072cb5195c32a3f2f))
- Add the Bee skeleton that reports ACPv2 as not implemented - ([ba0b517](https://github.com/noirbizarre/bee/commit/ba0b517b0fa83533d5e8d6dae5bf900402469149))

### 🐛 Bug Fixes

- **acp** Preserve unknown prompt content safely ([#95](https://github.com/noirbizarre/bee/issues/95)) - ([eca3b80](https://github.com/noirbizarre/bee/commit/eca3b801241c540e0910ba95316e30c3cfdaac50))
- **acp** Accept start resume cursors and complete compatibility checks ([#50](https://github.com/noirbizarre/bee/issues/50)) - ([4303174](https://github.com/noirbizarre/bee/commit/430317465ea7bf6fb1d1c4b316edefe5980b5e22))
- **auth** Reject credential stores that cannot be read back - ([c0d8be3](https://github.com/noirbizarre/bee/commit/c0d8be32212b66182768d29f3986bcdac1ec85d6))
- **auth** Complete documented OpenRouter authentication ([#62](https://github.com/noirbizarre/bee/issues/62)) - ([ae04bda](https://github.com/noirbizarre/bee/commit/ae04bda8676c4e1575221f3a93db751dc51d612a))
- **ci** Reduce coverage build debug information - ([404579d](https://github.com/noirbizarre/bee/commit/404579dab56c15074c3a79a334cdf6b26229f379))
- **ci** Lock mise tools for every platform CI runs on - ([5d53245](https://github.com/noirbizarre/bee/commit/5d53245b6695ed67a643387dff0ce46432cf1a4b))
- **http** Retain status classification for oversized error bodies - ([d2793fc](https://github.com/noirbizarre/bee/commit/d2793fcd142b0130d8677040113e6f2d37548fac))
- **llm** Make chat refusal classification authoritative - ([7960c4f](https://github.com/noirbizarre/bee/commit/7960c4f11ff37ba2ccf4c60d508fdbc6a1e0222c))
- **provider** Redact configuration values from debug output - ([4b4bb96](https://github.com/noirbizarre/bee/commit/4b4bb965f553a6b5db75843ec382643afb208d0e))
- **tests** Decouple authentication tests from Unix storage - ([41efd0d](https://github.com/noirbizarre/bee/commit/41efd0dbcfe72f54ef6bf8d16b207ae4b51b84b5))
- **tests** Compare preset origins as paths - ([56a9f70](https://github.com/noirbizarre/bee/commit/56a9f70f15cd7d4ecd03afb9ae7322cf38242a49))
- **tests** Make configuration assertions portable - ([bd46b67](https://github.com/noirbizarre/bee/commit/bd46b67dd6be4fb0a4e32e2b1e86f5639b46b9fa))
- **tools** Preserve directory permissions during recursive copy - ([7517dad](https://github.com/noirbizarre/bee/commit/7517dad35a0ca45c9410a3963e70a37a9ba14d9b))
- Reconcile runtime invariants and documentation ([#98](https://github.com/noirbizarre/bee/issues/98)) - ([f0feb4e](https://github.com/noirbizarre/bee/commit/f0feb4ea8118cd3978c66e342c26305a2ef521d7))

### 📚 Documentation

- **acp** Document complete protocol compatibility matrix ([#90](https://github.com/noirbizarre/bee/issues/90)) - ([1a65fe3](https://github.com/noirbizarre/bee/commit/1a65fe3822375a13ce89f7434d0553264c9a5de3))
- **auth** Refresh login mechanism architecture comments - ([3213830](https://github.com/noirbizarre/bee/commit/3213830a745d8625d69bccfc48ea92351860ea63))
- **auth** Distinguish callback and device login requirements - ([d446af7](https://github.com/noirbizarre/bee/commit/d446af7a8607276ba035b715ffc25eaed60e2ccc))
- **auth** Describe copilot authentication header overrides - ([987e4ab](https://github.com/noirbizarre/bee/commit/987e4abe9e64159d8844e329509bb2fb754cbdbc))
- **compatibility** Correct terminal update implementation status - ([f332305](https://github.com/noirbizarre/bee/commit/f332305cf4fab381eca2599cc54c7b035881eea7))
- **config** Wrap login guidance to satisfy markdown lint - ([59901ab](https://github.com/noirbizarre/bee/commit/59901ab60d866b7bc46cbe2c8e782ab5ffbd4f28))
- **config** Correct new-session selection precedence - ([074aa8f](https://github.com/noirbizarre/bee/commit/074aa8f8badee11fc02a2cb6b28fb1d2b9d63488))
- **inference** Clarify unknown prompt content handling - ([f143ebd](https://github.com/noirbizarre/bee/commit/f143ebd0ca9e6cba43f3d830a0d78e10097053aa))
- **mcp** Clarify remote tool path semantics - ([065381a](https://github.com/noirbizarre/bee/commit/065381a4ca9a744ffeaafe17593c49b50669c3cd))
- **profiles** Document explicit login onboarding exception - ([fda6684](https://github.com/noirbizarre/bee/commit/fda66845b2d2f0e3361b753a6b5403949a5612c5))
- **sessions** Clarify removed-connection recovery requirements - ([ffdad20](https://github.com/noirbizarre/bee/commit/ffdad209d2961244929808d4b4ee69a3560af461))
- Prepare configuration and onboarding for 0.1.0 ([#99](https://github.com/noirbizarre/bee/issues/99)) - ([9f8ebf3](https://github.com/noirbizarre/bee/commit/9f8ebf3dd2ec93cc2d3954f6f00fe8830719aeec))
- Add Bee branding to README and documentation ([#34](https://github.com/noirbizarre/bee/issues/34)) - ([8800a9f](https://github.com/noirbizarre/bee/commit/8800a9f808781f94363ff7b4bf9ea7a3f6f2977f))

### 🧪 Tests

- **acp** Add compatibility and end-to-end coverage ([#47](https://github.com/noirbizarre/bee/issues/47)) - ([3039628](https://github.com/noirbizarre/bee/commit/3039628b3ab179611c5da020039e899542f5475b))

### 🧹 Chores

- Initialize from rust.tpl - ([03cfb93](https://github.com/noirbizarre/bee/commit/03cfb93484f246368ca216d563e5902a669665c9))

### Tpl

- Render rust at main - ([ad51914](https://github.com/noirbizarre/bee/commit/ad519142e2721d9875cd0cfbb1eb2152e1dd3c9d))

## ❤️ New Contributors

* @noirbizarre made their first contribution in [#99](https://github.com/noirbizarre/bee/pull/99)
