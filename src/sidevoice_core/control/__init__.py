"""The node's control plane: its conversations, their history, who listens to what, and the link
with this machine's connector. One per node.

It creates one pipeline per call (`calls.run_call` → `sidevoice_core.pipeline.call.CallPipeline`)
and may import the pipeline; the pipeline never imports it. Neither imports a web framework: what
carries a request is `sidevoice_core.server`'s business, and a refusal here is a `Refusal`, not an
HTTP error.
"""
