# Speech clips for the ducking bench

Two utterances of LibriSpeech (speaker 1272, `dev-clean`), used by the
ducking bench (`src/duck_bench.rs`) as real speech:

| File | LibriSpeech id | Transcript | Length |
|------|----------------|------------|--------|
| `1272-128104-0001.s16` | 1272-128104-0001 | NOR IS MISTER QUILTER'S MANNER LESS INTERESTING THAN HIS MATTER | 4.8 s |
| `1272-135031-0004.s16` | 1272-135031-0004 | THE KING HAS FLED IN DISGRACE AND YOUR FRIENDS ARE ASKING FOR YOU | 4.2 s |

Format: raw signed 16-bit little-endian PCM, mono, 16 kHz (the corpus' own
rate), no header. The bench upsamples them to 48 kHz.

Source: LibriSpeech ASR corpus, Vassil Panayotov, Guoguo Chen, Daniel Povey
and Sanjeev Khudanpur, <https://www.openslr.org/12>, licensed under
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/). Taken from the
FLAC files of the `hf-internal-testing/librispeech_asr_dummy` copy
(`clean/validation-00000-of-00001.parquet`, rows 1 and 19,
<https://huggingface.co/datasets/hf-internal-testing/librispeech_asr_dummy>)
and converted unchanged with
`ffmpeg -i <id>.flac -f s16le -ac 1 -ar 16000 <id>.s16`.
