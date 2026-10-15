T=$AUDIT_TOOLS
sh $T/run_ui.sh U4_ui_nogame_unmuted nogame 30 60
MUTED=true sh $T/run_ui.sh U5_ui_nogame_muted nogame 30 60
sh $T/run_ui.sh M1_ui_record10min_replay60 record 630 60
