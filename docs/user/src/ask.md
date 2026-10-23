# Ask Parley

**Ask Parley** answers plain-language questions across all your meetings, such as *"What did we decide about pricing?"* or *"Which action items are still unresolved?"*. Every answer cites the meetings and moments it came from: click a citation to open the meeting at that point.

![Ask Parley](/images/ask.png)

To ask about a single meeting, use the **Ask** tab on the meeting instead.

## Narrowing the search

Use the filters above the question box to limit which meetings are searched:

- **Person**: meetings with a particular [speaker](speakers.md).
- **Date**: the past week, month, three months or year, this year, or a custom range.
- **Series**: one recurring calendar meeting.

Click **New chat** to start again. Conversations here aren't saved once you quit Parley.

## Keyword and semantic search

Parley first finds the parts of your meetings that are relevant to the question, then has your AI model answer from them.

- **Keyword search** always works.
- **Semantic search** also finds passages that use different words from your question ("budget" for "how much will it cost"). It runs on your computer with a small built-in model (about 146 MB), which Parley downloads in the background shortly after you first start it. There's nothing to set up.

While Ask Parley is open, Parley indexes new meetings for semantic search in the background. The line under the *Ask Parley* heading tells you which kind of search is in use and how far indexing has got.

If you use [Ollama](models.md#ollama) as your summary provider, Parley uses Ollama for semantic search instead; run `ollama pull nomic-embed-text` once for it.

Answers are written by the AI model chosen in [Settings → Summary](models.md#language-model-for-summaries-and-ask). With a cloud provider, the matching transcript passages are sent to it.
