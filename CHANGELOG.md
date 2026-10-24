# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.1.0 - 2026-10-24

### 💫 Features

- **acp** Implement permission and elicitation interactions ([#48](https://github.com/noirbizarre/bee/issues/48)) - ([40fcb30](https://github.com/noirbizarre/bee/commit/40fcb304dcd778852f875600959be421c2edf20e))
- **acp** Cancel running prompts and map protocol errors - ([d752314](https://github.com/noirbizarre/bee/commit/d75231449e454944565ec6bed590ef9e95b736df))
- **acp** Implement the session and prompt lifecycle - ([3e72da2](https://github.com/noirbizarre/bee/commit/3e72da205742bf4ecb8da6af6b4f877b83faa862))
- **acp** Serve ACPv2 over stdio - ([55a864b](https://github.com/noirbizarre/bee/commit/55a864bfb20dcfcfb6a1ec867fb5d706298968b5))
- **acp** Validate MCP-over-ACP tool invocation - ([baa6791](https://github.com/noirbizarre/bee/commit/baa6791fb6803c671ac0275437242c20d7b8c3d5))
- **acp** Validate the ACPv2 Rust SDK integration - ([87a2a4b](https://github.com/noirbizarre/bee/commit/87a2a4b3fdc1624097ba6cefb593c0416ad4647f))
- **agent** Complete bounded native tool execution ([#41](https://github.com/noirbizarre/bee/issues/41)) - ([0ca857e](https://github.com/noirbizarre/bee/commit/0ca857e0b17920a371a524b18c2c882daddf61f7))
- **auth** Authenticate providers with env, stored and browser credentials - ([b5f0786](https://github.com/noirbizarre/bee/commit/b5f0786bf328e9d0783af336c13b1a49740d80fd))
- **config** Layer configuration from XDG, BEE_CONFIG and the project - ([f0f6e75](https://github.com/noirbizarre/bee/commit/f0f6e75fb0626f76db7bb369da5250751932036a))
- **context** Manage conversation history and token limits ([#43](https://github.com/noirbizarre/bee/issues/43)) - ([a7270a6](https://github.com/noirbizarre/bee/commit/a7270a637579bf7734f31dd1f3cdae0e5649dc30))
- **instructions** Support AGENTS.md and standard Agent Skills ([#45](https://github.com/noirbizarre/bee/issues/45)) - ([a71245d](https://github.com/noirbizarre/bee/commit/a71245d908cb77c7a17f73bc2a907e5cc7635694))
- **llm** Implement Anthropic Messages inference - ([a310d9d](https://github.com/noirbizarre/bee/commit/a310d9dda6fce323c1005fb54cf92dc6b9fc3c03))
- **llm** Implement OpenAI Responses inference - ([22d79c8](https://github.com/noirbizarre/bee/commit/22d79c8b63e25b7237011e04671e983f0f652e19))
- **llm** Implement Chat Completions inference - ([94f8ec9](https://github.com/noirbizarre/bee/commit/94f8ec924ad50a61eab09030bf7b89e12166d3f4))
- **mcp** Integrate client-hosted tools through ACPv2 ([#44](https://github.com/noirbizarre/bee/issues/44)) - ([a5345e1](https://github.com/noirbizarre/bee/commit/a5345e16591d934f03c027b40c84a8741d25f1bc))
- **preset** Describe provider presets and login flows as data - ([bbd9887](https://github.com/noirbizarre/bee/commit/bbd98870bcbf1efbd48bd5141f6e5afa7d65234f))
- **provider** Select inference protocols per model ([#38](https://github.com/noirbizarre/bee/issues/38)) - ([754a5ec](https://github.com/noirbizarre/bee/commit/754a5ec945138b1d900740315f27a6fd1c1c444f))
- **provider** Add Responses protocol and well-known provider presets - ([efe66fc](https://github.com/noirbizarre/bee/commit/efe66fcc07afbc0066fabaca9b2d4e89ef9d5a47))
- **provider**  🚨 **breaking** Let a provider speak several protocols - ([e2ca05d](https://github.com/noirbizarre/bee/commit/e2ca05d2b3f1a620625d007d2eca9f85c6c8cf4c))
- **provider** Configure provider profiles and select them per session - ([c7d642e](https://github.com/noirbizarre/bee/commit/c7d642ee948c05b4ff8ec202d712a3670ac675ec))
- **tools** Add native Bash command execution ([#42](https://github.com/noirbizarre/bee/issues/42)) - ([94510aa](https://github.com/noirbizarre/bee/commit/94510aabf190b91e22a878fa260422e72cc97721))
- **tools** Add native write and edit tools ([#40](https://github.com/noirbizarre/bee/issues/40)) - ([aa89fad](https://github.com/noirbizarre/bee/commit/aa89fad7f8c8764aeb3e9d0a102d563cb7be2f4a))
- **tools** Add native file reading and discovery ([#39](https://github.com/noirbizarre/bee/issues/39)) - ([a3b825b](https://github.com/noirbizarre/bee/commit/a3b825bee81483faaad449700225c79c6827a6b8))
- **tools** Define native tool execution interface ([#37](https://github.com/noirbizarre/bee/issues/37)) - ([e907520](https://github.com/noirbizarre/bee/commit/e907520d996b6066d75384a072cb5195c32a3f2f))
- Add the Bee skeleton that reports ACPv2 as not implemented - ([ba0b517](https://github.com/noirbizarre/bee/commit/ba0b517b0fa83533d5e8d6dae5bf900402469149))

### 🐛 Bug Fixes

- **ci** Reduce coverage build debug information - ([404579d](https://github.com/noirbizarre/bee/commit/404579dab56c15074c3a79a334cdf6b26229f379))
- **ci** Lock mise tools for every platform CI runs on - ([5d53245](https://github.com/noirbizarre/bee/commit/5d53245b6695ed67a643387dff0ce46432cf1a4b))
- **tests** Decouple authentication tests from Unix storage - ([41efd0d](https://github.com/noirbizarre/bee/commit/41efd0dbcfe72f54ef6bf8d16b207ae4b51b84b5))
- **tests** Compare preset origins as paths - ([56a9f70](https://github.com/noirbizarre/bee/commit/56a9f70f15cd7d4ecd03afb9ae7322cf38242a49))
- **tests** Make configuration assertions portable - ([bd46b67](https://github.com/noirbizarre/bee/commit/bd46b67dd6be4fb0a4e32e2b1e86f5639b46b9fa))

### 📚 Documentation

- Add Bee branding to README and documentation ([#34](https://github.com/noirbizarre/bee/issues/34)) - ([8800a9f](https://github.com/noirbizarre/bee/commit/8800a9f808781f94363ff7b4bf9ea7a3f6f2977f))

### 🧪 Tests

- **acp** Add compatibility and end-to-end coverage ([#47](https://github.com/noirbizarre/bee/issues/47)) - ([3039628](https://github.com/noirbizarre/bee/commit/3039628b3ab179611c5da020039e899542f5475b))

### 🧹 Chores

- Initialize from rust.tpl - ([03cfb93](https://github.com/noirbizarre/bee/commit/03cfb93484f246368ca216d563e5902a669665c9))

### Tpl

- Render rust at main - ([ad51914](https://github.com/noirbizarre/bee/commit/ad519142e2721d9875cd0cfbb1eb2152e1dd3c9d))

## ❤️ New Contributors

* @noirbizarre made their first contribution in [#48](https://github.com/noirbizarre/bee/pull/48)
