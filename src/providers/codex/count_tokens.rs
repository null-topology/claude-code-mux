use super::translate::request::{
    ResponsesContentPart, ResponsesFunctionCallOutput, ResponsesFunctionCallOutputContentPart,
    ResponsesInputItem, ResponsesRequest, ResponsesTool,
};
use tiktoken_rs::o200k_base_singleton;

/// Approximate token counter for Codex translated requests.
/// Text uses the OpenAI `o200k_base` tokenizer. Images, encrypted reasoning,
/// and protocol framing still use local estimates, so the result is not a
/// provider billing count.
pub fn count_translated_tokens(translated: &ResponsesRequest) -> u64 {
    let mut total = 0u64;

    // Instructions
    if let Some(ref instructions) = translated.instructions {
        total += approx_token_count(instructions);
    }

    // Input items
    for item in &translated.input {
        total += count_input_item_tokens(item);
    }

    // Tools
    if let Some(ref tools) = translated.tools {
        total += count_tool_tokens(tools);
    }

    // Overhead
    total += translated.input.len() as u64 * 4;
    total += translated.tools.as_ref().map_or(0, |t| t.len() as u64 * 4);

    // Model name
    total += approx_token_count(&translated.model);

    total.max(1)
}

fn count_input_item_tokens(item: &ResponsesInputItem) -> u64 {
    match item {
        ResponsesInputItem::AdditionalTools { tools, .. } => tools
            .iter()
            .map(|tool| approx_token_count(&serde_json::to_string(tool).unwrap_or_default()))
            .sum(),
        ResponsesInputItem::Message { content, .. } => {
            let mut total = 0u64;
            for part in content {
                total += count_content_part_tokens(part);
            }
            total
        }
        ResponsesInputItem::FunctionCall {
            name, arguments, ..
        } => approx_token_count(name) + approx_token_count(arguments),
        ResponsesInputItem::FunctionCallOutput { output, .. } => {
            count_function_call_output_tokens(output)
        }
        ResponsesInputItem::ToolSearchCall { arguments, .. } => {
            approx_token_count(&serde_json::to_string(arguments).unwrap_or_default())
        }
        ResponsesInputItem::ToolSearchOutput { tools, .. } => tools
            .iter()
            .map(|tool| approx_token_count(&serde_json::to_string(tool).unwrap_or_default()))
            .sum(),
        ResponsesInputItem::Reasoning {
            encrypted_content, ..
        }
        | ResponsesInputItem::Compaction { encrypted_content } => {
            approx_reasoning_token_count(encrypted_content)
        }
        ResponsesInputItem::CompactionTrigger => 0,
    }
}

fn count_function_call_output_tokens(output: &ResponsesFunctionCallOutput) -> u64 {
    match output {
        ResponsesFunctionCallOutput::Text(text) => approx_token_count(text),
        ResponsesFunctionCallOutput::ContentItems(content) => content
            .iter()
            .map(|part| match part {
                ResponsesFunctionCallOutputContentPart::InputText { text } => {
                    approx_token_count(text)
                }
                ResponsesFunctionCallOutputContentPart::InputImage { .. } => 2000,
            })
            .sum(),
    }
}

fn approx_reasoning_token_count(encoded_content: &str) -> u64 {
    let model_visible_bytes = encoded_content
        .len()
        .saturating_mul(3)
        .checked_div(4)
        .unwrap_or(0)
        .saturating_sub(650);
    u64::try_from(model_visible_bytes.saturating_add(3) / 4).unwrap_or(u64::MAX)
}

fn count_content_part_tokens(part: &ResponsesContentPart) -> u64 {
    match part {
        ResponsesContentPart::InputText { text } => approx_token_count(text),
        ResponsesContentPart::OutputText { text } => approx_token_count(text),
        ResponsesContentPart::InputImage { .. } => 2000, // Image token estimate
    }
}

fn count_tool_tokens(tools: &[ResponsesTool]) -> u64 {
    let mut total = 0u64;
    for tool in tools {
        match tool {
            ResponsesTool::Function(f) => {
                total += approx_token_count(&f.name);
                if let Some(ref desc) = f.description {
                    total += approx_token_count(desc);
                }
                total +=
                    approx_token_count(&serde_json::to_string(&f.parameters).unwrap_or_default());
            }
            ResponsesTool::WebSearch(_) => {
                total += 10; // fixed overhead for web search tool
            }
            ResponsesTool::ToolSearch(t) => {
                if let Some(ref desc) = t.description {
                    total += approx_token_count(desc);
                }
                total +=
                    approx_token_count(&serde_json::to_string(&t.parameters).unwrap_or_default());
            }
        }
    }
    total
}

pub(crate) fn approx_token_count(text: &str) -> u64 {
    u64::try_from(o200k_base_singleton().count_ordinary(text)).unwrap_or(u64::MAX)
}

pub(crate) fn truncate_to_token_budget(text: &str, max_tokens: u64) -> String {
    let max_tokens = usize::try_from(max_tokens).unwrap_or(usize::MAX);
    if max_tokens == 0 {
        return String::new();
    }

    let tokenizer = o200k_base_singleton();
    let tokens = tokenizer.encode_ordinary(text);
    if tokens.len() <= max_tokens {
        return text.to_string();
    }

    // A token boundary may split a multi-byte UTF-8 scalar. Back up until the
    // decoded prefix is valid rather than replacing bytes or splitting text.
    for end in (1..=max_tokens).rev() {
        if let Ok(prefix) = tokenizer.decode(&tokens[..end]) {
            return prefix;
        }
    }

    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn count_simple_request() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"},
        }))
        .unwrap();
        let count = count_translated_tokens(&req);
        assert!(count > 0);
    }

    #[test]
    fn count_request_with_tools() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "use tool"}]}],
            "tools": [{"type": "function", "name": "search", "parameters": {"type": "object"}}],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"},
        }))
        .unwrap();
        let count = count_translated_tokens(&req);
        assert!(count > 0);
    }

    #[test]
    fn count_is_monotonic() {
        let short: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"},
        }))
        .unwrap();
        let long: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "this is a much longer message with many words in it"}]}],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"},
        }))
        .unwrap();
        assert!(count_translated_tokens(&long) >= count_translated_tokens(&short));
    }

    #[test]
    fn o200k_counts_unbroken_text_instead_of_collapsing_it() {
        assert_eq!(approx_token_count(&"a".repeat(4096)), 512);
        assert_eq!(approx_token_count(&"汉字测试上下文压缩".repeat(384)), 2688);
        assert_eq!(
            approx_token_count(&"QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo".repeat(112)),
            2240
        );
        assert_eq!(
            approx_token_count(&"function f(a){return a===0?1:a*f(a-1)};".repeat(96)),
            1632
        );
    }

    #[test]
    fn truncation_respects_o200k_budget_and_utf8_boundaries() {
        for text in [
            "a".repeat(4096),
            "汉字测试上下文压缩".repeat(384),
            "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo".repeat(112),
        ] {
            let truncated = truncate_to_token_budget(&text, 100);
            assert!(text.starts_with(&truncated));
            assert!(approx_token_count(&truncated) <= 100);
            assert!(!truncated.is_empty());
        }
    }

    #[test]
    fn structured_tool_output_counts_text_and_images() {
        let text = ResponsesFunctionCallOutput::Text("caption".to_string());
        let mixed = ResponsesFunctionCallOutput::ContentItems(vec![
            ResponsesFunctionCallOutputContentPart::InputText {
                text: "caption".to_string(),
            },
            ResponsesFunctionCallOutputContentPart::InputImage {
                image_url: "data:image/png;base64,YQ==".to_string(),
                detail: None,
            },
        ]);

        assert_eq!(
            count_function_call_output_tokens(&mixed),
            count_function_call_output_tokens(&text) + 2000
        );
    }

    #[test]
    fn encrypted_reasoning_uses_codex_model_visible_size_estimate() {
        let encoded_content = "A".repeat(4000);
        assert_eq!(approx_reasoning_token_count(&encoded_content), 588);
        assert_eq!(approx_reasoning_token_count("short"), 0);
    }

    const TOOL_SEARCH_DESCRIPTION: &str = "Search the deferred tools";

    fn tool_search_call_arguments() -> serde_json::Value {
        json!({"query": "read a file"})
    }

    fn tool_search_loaded_tools() -> Vec<serde_json::Value> {
        vec![json!({
            "type": "function",
            "name": "Read",
            "description": "Read a file from disk",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
            "defer_loading": true,
        })]
    }

    fn tool_search_tool_parameters() -> serde_json::Value {
        json!({"type": "object", "properties": {"query": {"type": "string"}}})
    }

    /// A translated request with a client-executed tool search replayed in the
    /// history, or the same request without any of it.
    fn tool_search_request(with_tool_search: bool) -> ResponsesRequest {
        let mut input = vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "load a tool"}],
        })];
        let mut tools = vec![json!({
            "type": "function",
            "name": "Bash",
            "parameters": {"type": "object"},
        })];
        if with_tool_search {
            input.push(json!({
                "type": "tool_search_call",
                "call_id": "toolsearch_1",
                "execution": "client",
                "status": "completed",
                "arguments": tool_search_call_arguments(),
            }));
            input.push(json!({
                "type": "tool_search_output",
                "call_id": "toolsearch_1",
                "status": "completed",
                "execution": "client",
                "tools": tool_search_loaded_tools(),
            }));
            tools.push(json!({
                "type": "tool_search",
                "execution": "client",
                "description": TOOL_SEARCH_DESCRIPTION,
                "parameters": tool_search_tool_parameters(),
            }));
        }
        serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": input,
            "tools": tools,
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"},
        }))
        .unwrap()
    }

    #[test]
    fn tool_search_items_and_tool_add_their_serialized_size() {
        let with = tool_search_request(true);
        let without = tool_search_request(false);

        assert!(matches!(
            with.input[1],
            ResponsesInputItem::ToolSearchCall { .. }
        ));
        assert!(matches!(
            with.input[2],
            ResponsesInputItem::ToolSearchOutput { .. }
        ));
        assert!(matches!(
            with.tools.as_ref().unwrap()[1],
            ResponsesTool::ToolSearch(_)
        ));

        // The counting rule: a tool search call contributes only its
        // `arguments` JSON, its output only the JSON of each loaded tool spec,
        // and the tool definition its description plus its `parameters` JSON.
        // `call_id`, `status`, `execution` and the tool's `type` add nothing
        // beyond the flat 4-token framing charged per input item and per tool.
        let expected_delta =
            approx_token_count(&serde_json::to_string(&tool_search_call_arguments()).unwrap())
                + tool_search_loaded_tools()
                    .iter()
                    .map(|tool| approx_token_count(&serde_json::to_string(tool).unwrap()))
                    .sum::<u64>()
                + approx_token_count(TOOL_SEARCH_DESCRIPTION)
                + approx_token_count(
                    &serde_json::to_string(&tool_search_tool_parameters()).unwrap(),
                )
                + 2 * 4
                + 4;

        let with_count = count_translated_tokens(&with);
        let without_count = count_translated_tokens(&without);
        assert!(with_count > without_count);
        assert_eq!(with_count, without_count + expected_delta);
    }

    #[test]
    fn tool_search_estimate_is_stable_across_identical_calls() {
        let req = tool_search_request(true);
        let first = count_translated_tokens(&req);
        assert!(first > 0);
        assert_eq!(first, count_translated_tokens(&req));
        assert_eq!(first, count_translated_tokens(&tool_search_request(true)));
    }

    #[test]
    fn empty_tool_search_shapes_count_without_panicking() {
        // A search still running (`arguments: null`) and one that loaded no
        // tool, with an empty tools list. An entirely absent `arguments` key
        // fails to deserialize, so it never reaches the counter.
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [
                {
                    "type": "tool_search_call",
                    "call_id": "",
                    "execution": "client",
                    "status": "in_progress",
                    "arguments": null,
                },
                {
                    "type": "tool_search_output",
                    "call_id": "",
                    "status": "completed",
                    "execution": "client",
                    "tools": [],
                }
            ],
            "tools": [],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"},
        }))
        .unwrap();

        // Only the serialized `null`, the two per-item overheads and the model
        // name remain; an output that loaded nothing contributes no content.
        assert_eq!(
            count_translated_tokens(&req),
            approx_token_count("null") + 2 * 4 + approx_token_count("gpt-5.5")
        );
    }
}
