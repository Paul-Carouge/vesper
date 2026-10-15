T=$AUDIT_TOOLS
sh $T/run_ui.sh U1_ui_nogame_replay60 nogame 30 60
sh $T/run_ui.sh U1b_ui_nogame_replay144 nogame 30 144
sh $T/run_ui.sh U2_ui_game_replay60 replay 30 60
sh $T/run_ui.sh U3_ui_game_record60 record 40 60
sh $T/run_game.sh D0_4k_g144_dormant 3840 2160 60 144 25 dormant
sh $T/run_game.sh D1_4k_g144_nolayer 3840 2160 60 144 25 nolayer
sh $T/run_game.sh A2_4k60_g144 3840 2160 60 144 30 rec
sh $T/run_game.sh B3_1440p144_g144 2560 1440 144 144 30 rec
sh $T/run_game.sh S3_4k60_g144_shimbt 3840 2160 60 144 15 recshim
sh $T/run_obs.sh O1_obs_4k60_g144 3840 2160 60 144 30
sh $T/run_obs.sh O2_obs_1440p144_g144 2560 1440 144 144 30
