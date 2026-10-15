import {Config} from '@remotion/cli/config';

// PNG frames keep the type crisp before the h264 encode.
Config.setVideoImageFormat('png');
Config.setOverwriteOutput(true);
Config.setEntryPoint('src/index.ts');
