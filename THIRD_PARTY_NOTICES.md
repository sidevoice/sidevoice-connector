# Third-party components

Sidevoice's MIT license applies to its own code. Dependencies and models keep
their original licenses. This repository does not bundle weights or generated
browser assets.

Major components include Pipecat, Transformers.js, ONNX Runtime, Kokoro-82M and
its ONNX conversion, eSpeak NG, the OpenTelemetry SDKs for Python and the browser
(Apache-2.0), and the MCP TypeScript SDK in the Claude draft.
Earlier experiments reference MLX Audio, Pocket TTS and FluidAudio.

In particular, eSpeak NG has GPL license obligations when redistributing its
generated WASM/bundles. Review upstream licenses and model cards before
redistributing compiled assets or weights; the repository's MIT license does
not replace those terms.

- https://github.com/pipecat-ai/pipecat
- https://github.com/huggingface/transformers.js
- https://github.com/microsoft/onnxruntime
- https://huggingface.co/hexgrad/Kokoro-82M
- https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX
- https://github.com/espeak-ng/espeak-ng
- https://github.com/open-telemetry/opentelemetry-python
- https://github.com/open-telemetry/opentelemetry-js
- https://github.com/modelcontextprotocol/typescript-sdk
