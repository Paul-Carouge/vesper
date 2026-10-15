T=$AUDIT_TOOLS
sh $T/run_game.sh A_4k60_g144 3840 2160 60 144 30 rec
sh $T/run_game.sh B_1440p144_g144 2560 1440 144 144 30 rec
sh $T/run_game.sh B2_1440p144_g240 2560 1440 144 240 30 rec
sh $T/run_game.sh C0_4k_uncapped_nolayer 3840 2160 60 0 25 nolayer
sh $T/run_game.sh C1_4k_uncapped_dormant 3840 2160 60 0 25 dormant
sh $T/run_game.sh C2_4k_uncapped_rec60 3840 2160 60 0 25 rec
sh $T/run_game.sh S1_4k60_g144_shim15 3840 2160 60 144 15 recshim
sh $T/run_game.sh S2_4k60_g144_shim35 3840 2160 60 144 35 recshim
