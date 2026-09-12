import { writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// Generated only in the browser harness's temporary library. The original
// movie sidecars remain first, and no extra title changes library/queue tests.
export const captionConversionFixtures = [
  {
    language: "zza", name: "SAMI clear and terminal gaps", extension: "smi",
    input: "<SAMI><SYNC Start=1000><P>First</P><SYNC Start=2000><P>&#160; &#xA0;</P><SYNC Start=5000><P>Next</P><SYNC Start=6500><P>&nbsp;</P></SAMI>",
    cues: [{ start: 1, end: 2, text: "First" }, { start: 5, end: 6.5, text: "Next" }],
    active: [[1.5, "First"], [2.25, ""], [4.9, ""], [5.25, "Next"], [6.75, ""]],
  },
  {
    language: "zzb", name: "SAMI literal markup and single entity decoding", extension: "smi",
    input: "<SAMI><!-- <SYNC Start=0>ignored --><SyNc title='x > y' START = \"1250\"><P><b>Bold</b> &lt;b&gt;literal&lt;/b&gt; &amp;lt;i&amp;gt;<bR />Tom &amp; Jerry &#60;3&#x3E; &quot;yes&quot; &apos;ok&apos; &unknown;<P>2 < 3 > 1<SYNC Start=2750><P>&nbsp;</P></SAMI>",
    cues: [{ start: 1.25, end: 2.75, text: 'Bold <b>literal</b> &lt;i&gt;\nTom & Jerry <3> "yes" \'ok\' &unknown;\n2 < 3 > 1' }],
  },
  {
    language: "zzc", name: "ASS literal markup, entities, overrides, and breaks", extension: "ass",
    input: "[Events]\nFoRmAt: Start, End, Text\nDiAlOgUe: 0:00:01.25,0:00:02.75,{\\i1}Italic{\\i0} <b>literal</b> &lt;3&gt; {braces}\\Nnext\\nline\\hword, comma",
    cues: [{ start: 1.25, end: 2.75, text: "Italic <b>literal</b> &lt;3&gt; {braces}\nnext\nline\u00a0word, comma" }],
  },
  {
    language: "zzd", name: "WebVTT BOM, CRLF, header metadata, blocks, IDs, and settings", extension: "vtt",
    input: "\ufeffWEBVTT\tCaption --> annotation\r\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:0\r\n\r\nNOTE commentary\r\nNot a cue\r\n\r\nSTYLE\r\n::cue { color: lime; }\r\n\r\nREGION\r\nid:bottom\r\nwidth:80%\r\n\r\nopening\r\n00:01.250\t-->\t00:02.750 line:90% position:20%\r\n<v Alice><b>Hello</b> &amp; <c.color>world</c></v>\r\n\r\nNOTE End\r\n\r\n00:02.500 --> 00:03.000\r\nOverlapping <00:02.750>text\r\n",
    cues: [{ start: 1.25, end: 2.75, text: "Hello & world", id: "opening", bold: "Hello", line: 90, position: 20 }, { start: 2.5, end: 3, text: "Overlapping text" }],
  },
  {
    language: "zze", name: "valid empty WebVTT", extension: "vtt",
    input: "WEBVTT\n\nNOTE No captions in this segment\n", cues: [],
  },
  {
    language: "zzf", name: "SubRip multiline unicode, formatting, and overlap", extension: "srt",
    input: "1\r\n00:00:01,250 --> 00:00:02,750\r\n<b>Hello</b> &amp; 世界\r\nSecond line\r\n\r\n2\r\n00:00:02,500 --> 00:00:03,000\r\nOverlap\r\n",
    cues: [{ start: 1.25, end: 2.75, text: "Hello & 世界\nSecond line", bold: "Hello" }, { start: 2.5, end: 3, text: "Overlap" }],
  },
  {
    language: "zzg", name: "WebVTT CR lines and supported cue markup", extension: "vtt",
    input: "WEBVTT description\r\r00:01.000 --> 00:02.000\r<i>Italic</i> <u>underlined</u> &lt;b&gt;literal&lt;/b&gt;\rnext line\r",
    cues: [{ start: 1, end: 2, text: "Italic underlined <b>literal</b>\nnext line" }],
  },
  {
    language: "zzh", name: "SSA intentional blank lines", extension: "ssa",
    input: "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:01.00,0:00:02.00,First\\N\\NLast",
    cues: [{ start: 1, end: 2, text: "First\n\u00a0\nLast" }],
  },
  {
    language: "zzt", name: "SAMI numeric CR line endings remain in one cue", extension: "smi",
    input: "<SYNC Start=1000><P>First&#13;&#13;Last<SYNC Start=2000><P>&nbsp;",
    cues: [{ start: 1, end: 2, text: "First\n\u00a0\nLast" }],
  },
  ...[
    ["zzi", "signature suffix", "WEBVTTjunk\n\n00:01.000 --> 00:02.000\nBad\n"],
    ["zzj", "missing header separator", "WEBVTT\n00:01.000 --> 00:02.000\nBad\n"],
    ["zzk", "comma timestamp", "WEBVTT\n\n00:01,000 --> 00:02,000\nBad\n"],
    ["zzl", "timestamp without milliseconds", "WEBVTT\n\n00:01 --> 00:02\nBad\n"],
    ["zzm", "timestamp exponent", "WEBVTT\n\n00:1e0 --> 00:02.000\nBad\n"],
    ["zzn", "short timestamp component", "WEBVTT\n\n0:00:01.000 --> 00:00:02.000\nBad\n"],
    ["zzo", "timing missing separator whitespace", "WEBVTT\n\n00:01.000-->00:02.000\nBad\n"],
    ["zzp", "backwards timing", "WEBVTT\n\n00:02.000 --> 00:01.000\nBad\n"],
    ["zzq", "timestamp overflow", "WEBVTT\n\n5124095576031:00:00.000 --> 5124095576032:00:00.000\nBad\n"],
  ].map(([language, name, input]) => ({ language, name, input, extension: "vtt", error: "caption_malformed" })),
  { language: "zzr", name: "SAMI decreasing sync", extension: "smi", input: "<SYNC Start=2000>First<SYNC Start=1000>Next", error: "caption_malformed" },
  { language: "zzs", name: "ASS malformed timestamp", extension: "ass", input: "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:NaN,0:00:02.00,Bad", error: "caption_malformed" },
];

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  if (!process.argv[2]) throw new Error("A temporary browser-test library is required");
  for (const fixture of captionConversionFixtures) {
    await writeFile(join(process.argv[2], "video", `movie.${fixture.language}-conversion.${fixture.extension}`), fixture.input, { flag: "wx" });
  }
}
