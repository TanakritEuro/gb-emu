; Link Pong: two-player Pong over the Game Boy link cable, one paddle per
; Game Boy. Written for this emulator's link cable, but plain DMG code that
; should run on real hardware too. Build with RGBDS (see build.js).
;
; How the two Game Boys stay in step ("lockstep"): every frame they swap one
; byte over the cable, each player's buttons. Both then run exactly the same
; game step with the same two inputs, so the ball and the score come out the
; same on both screens without ever being sent.
;
; Who clocks the transfers: on the title screen both sides listen as slave
; (SC = $80) with READY in SB. Whoever presses START becomes master: it sends
; HELLO until the other answers READY, and both start the game. Inputs never
; use bit 7, so a master reading $FF knows nobody was listening yet (the
; other side was busy, or there's no cable) and tries again.
; https://gbdev.io/pandocs/Serial_Data_Transfer_(Link_Cable).html

; Hardware registers
DEF rP1   EQU $FF00
DEF rSB   EQU $FF01
DEF rSC   EQU $FF02
DEF rLCDC EQU $FF40
DEF rSCY  EQU $FF42
DEF rSCX  EQU $FF43
DEF rLY   EQU $FF44
DEF rBGP  EQU $FF47
DEF rOBP0 EQU $FF48
DEF rIE   EQU $FFFF

DEF LCDC_ON EQU %10010011 ; LCD on, tiles at $8000, map $9800, sprites, BG
DEF MAP EQU $9800
DEF OAM_ADDR EQU $FE00

; The input byte each side sends every frame. Bit 7 stays 0.
DEF IN_UP     EQU 0
DEF IN_DOWN   EQU 1
DEF IN_START  EQU 2
DEF IN_SELECT EQU 3
DEF IN_B      EQU 4

; Title screen handshake
DEF HELLO EQU $5A ; sent by the side that pressed START
DEF READY EQU $A5 ; what a listening title screen answers with

DEF ROLE_MASTER EQU 1 ; player 1, clocks the transfers
DEF ROLE_SLAVE  EQU 2 ; player 2
DEF ROLE_CPU    EQU 3 ; player 1 against the computer, no link

DEF ST_PLAY EQU 1
DEF ST_OVER EQU 2

; The court, in screen pixels
DEF TOP      EQU 16  ; below the score bar
DEF BOTTOM   EQU 144
DEF PADDLE_H EQU 24
DEF P1_X     EQU 8
DEF P2_X     EQU 144
DEF BALL_X0  EQU 76
DEF BALL_Y0  EQU 76
DEF PADDLE_Y0 EQU (TOP + BOTTOM) / 2 - PADDLE_H / 2
DEF SPEED    EQU 2
DEF WIN_SCORE EQU 9
DEF SERVE_WAIT EQU 60 ; frames the ball rests before each serve
DEF LINK_PATIENCE EQU 180 ; frames without a word before "LINK LOST"

; Tiles: 0 blank, 1-10 the digits, then letters (see Font), then these.
DEF T_COLON  EQU 31
DEF T_PADDLE EQU 32
DEF T_BALL   EQU 33
DEF T_NET    EQU 34
DEF T_BORDER EQU 35

; Strings use the tile numbers directly.
CHARMAP " ", 0
CHARMAP "0", 1
CHARMAP "1", 2
CHARMAP "2", 3
CHARMAP "3", 4
CHARMAP "4", 5
CHARMAP "5", 6
CHARMAP "6", 7
CHARMAP "7", 8
CHARMAP "8", 9
CHARMAP "9", 10
CHARMAP "A", 11
CHARMAP "B", 12
CHARMAP "C", 13
CHARMAP "E", 14
CHARMAP "F", 15
CHARMAP "G", 16
CHARMAP "I", 17
CHARMAP "K", 18
CHARMAP "L", 19
CHARMAP "M", 20
CHARMAP "N", 21
CHARMAP "O", 22
CHARMAP "P", 23
CHARMAP "R", 24
CHARMAP "S", 25
CHARMAP "T", 26
CHARMAP "U", 27
CHARMAP "V", 28
CHARMAP "W", 29
CHARMAP "Y", 30
CHARMAP ":", T_COLON

DEF END EQU $FF ; ends a string

MACRO text ; column, row, string label
    ld hl, MAP + 32 * (\2) + (\1)
    ld de, \3
    call PutText
ENDM

SECTION "Variables", WRAM0
wVars:
wRole:      db
wState:     db
wBallX:     db
wBallY:     db
wBallDX:    db
wBallDY:    db
wP1Y:       db
wP2Y:       db
wScore1:    db
wScore2:    db
wServeWait: db
wIn1:       db ; player 1's input this frame
wIn2:       db ; player 2's
wMyIn:      db ; this Game Boy's buttons
wPrevIn:    db ; ... last time they were read
wFrame:     db
wRedraw:    db ; the screen needs redrawing (new game, game over)
wGoTitle:   db ; back to the title screen
wTitleNote: db ; 1: say "LINK LOST" on the title screen
wVarsEnd:

SECTION "Header", ROM0[$100]
    nop
    jp Start
    ds $150 - @, 0 ; the header; rgbfix fills it in

SECTION "Main", ROM0

Start:
    di
    ld sp, $E000
    call LcdOff
    ; Every variable starts at 0.
    ld hl, wVars
    ld b, wVarsEnd - wVars
    xor a
.clear:
    ld [hli], a
    dec b
    jr nz, .clear
    ; The font is 1 bit per pixel; each row goes in twice (color 3).
    ld hl, $8000
    ld de, Font
    ld bc, FontEnd - Font
.tiles:
    ld a, [de]
    inc de
    ld [hli], a
    ld [hli], a
    dec bc
    ld a, b
    or c
    jr nz, .tiles
    ld a, %11100100
    ldh [rBGP], a
    ldh [rOBP0], a
    xor a
    ldh [rSCX], a
    ldh [rSCY], a
    ldh [rIE], a ; no interrupts: everything polls
    ; fall through to the title screen

; --- Title screen -----------------------------------------------------------

Title:
    call LcdOff
    call ClearMap
    call HideSprites
    xor a
    ld [wRole], a
    ld [wGoTitle], a
    text 5, 4, sTitle
    text 2, 9, sStart2P
    text 3, 11, sVsCpu
    ld a, [wTitleNote]
    and a
    jr z, .noNote
    text 5, 14, sLinkLost
    xor a
    ld [wTitleNote], a
.noNote:
    call LcdOn
    call ReadInput ; a START still held from before doesn't count

.listen:
    ; Wait to be clocked by a player pressing START on the other side.
    ld a, READY
    ldh [rSB], a
    ld a, $80
    ldh [rSC], a
.loop:
    call WaitVBlank
    ldh a, [rSC]
    bit 7, a
    jr nz, .notClocked
    ldh a, [rSB]
    cp HELLO
    jp z, StartAsSlave
    jr .listen ; something else came in: listen again
.notClocked:
    call ReadInput
    bit IN_START, a
    jr nz, .connect
    bit IN_SELECT, a
    jp nz, StartVsCpu
    jr .loop

.connect:
    ; This side clocks: say HELLO until the other answers READY. Between
    ; tries, listen too, in case both pressed START at once.
    text 3, 14, sWaiting
    text 5, 15, sCancel
.try:
    ld a, HELLO
    ldh [rSB], a
    ld a, $81
    ldh [rSC], a
.sending:
    ldh a, [rSC]
    bit 7, a
    jr nz, .sending
    ldh a, [rSB]
    cp READY
    jp z, StartAsMaster
    ld a, READY
    ldh [rSB], a
    ld a, $80
    ldh [rSC], a
    call WaitVBlank
    ldh a, [rSC]
    bit 7, a
    jr nz, .stillAlone
    ldh a, [rSB]
    cp HELLO
    jp z, StartAsSlave
.stillAlone:
    call ReadInput
    bit IN_B, a
    jr z, .try
    text 3, 14, sBlank14 ; cancelled
    text 5, 15, sBlank14
    jp .listen

StartAsMaster:
    ld a, ROLE_MASTER
    jr NewGame
StartAsSlave:
    ld a, ROLE_SLAVE
    jr NewGame
StartVsCpu:
    ld a, ROLE_CPU
    ; fall through

; --- Playing ----------------------------------------------------------------

NewGame:
    ld [wRole], a
    call ResetGame
    call DrawScreen
    ld a, [wRole]
    cp ROLE_SLAVE
    jr z, SlaveLoop
    cp ROLE_CPU
    jr z, CpuLoop
    ; fall through

; Player 1 with a partner: clocks the swap each frame.
MasterLoop:
    call WaitVBlank
    call Draw
    call ReadInput
    call MasterSwap
    jp c, LinkLost
    ld [wIn2], a
    ld a, [wMyIn]
    ld [wIn1], a
    call Step
    call AfterStep
    jr MasterLoop

; Player 2: offers its input before VBlank, so it's ready when the master
; clocks, then waits for the swap.
SlaveLoop:
    call ReadInput
    ld a, [wMyIn]
    ldh [rSB], a
    ld a, $80
    ldh [rSC], a
    call WaitVBlank
    call Draw
    call SlaveWait
    jp c, LinkLost
    ldh a, [rSB]
    ld [wIn1], a
    ld a, [wMyIn]
    ld [wIn2], a
    call Step
    call AfterStep
    jr SlaveLoop

; Against the computer: no link at all.
CpuLoop:
    call WaitVBlank
    call Draw
    call ReadInput
    ld a, [wMyIn]
    ld [wIn1], a
    call CpuInput
    ld [wIn2], a
    call Step
    call AfterStep
    jr CpuLoop

; Back to the title screen, or a new screen to draw, after a step. (Leaves
; through Title directly when asked to.)
AfterStep:
    ld a, [wGoTitle]
    and a
    jr nz, .title
    ld a, [wRedraw]
    and a
    ret z
    jp DrawScreen
.title:
    pop hl ; not returning to the loop
    jp Title

LinkLost:
    ld a, 1
    ld [wTitleNote], a
    jp Title

; Swaps this frame's input with player 2, which listens as slave.
; Out: a = player 2's input; carry set if it stopped answering.
MasterSwap:
    ld bc, 0 ; tries that found nobody listening
.try:
    ld a, [wMyIn]
    ldh [rSB], a
    ld a, $81
    ldh [rSC], a
.wait:
    ldh a, [rSC]
    bit 7, a
    jr nz, .wait
    ldh a, [rSB]
    bit 7, a
    jr z, .done ; a real input (bit 7 clear), not $FF
    inc bc
    ld a, b
    cp 12 ; about 3000 tries, around 3 s of link time
    jr c, .try
    scf
    ret
.done:
    and a ; carry clear
    ret

; Waits for player 1 to clock our input across. Carry set if it hasn't
; within LINK_PATIENCE frames.
SlaveWait:
    ld b, LINK_PATIENCE
.poll:
    ldh a, [rSC]
    bit 7, a
    jr z, .done
    ldh a, [rLY]
    cp 144
    jr nz, .poll
    dec b ; another frame went by
    jr z, .lost
.line144:
    ldh a, [rSC]
    bit 7, a
    jr z, .done
    ldh a, [rLY]
    cp 144
    jr z, .line144
    jr .poll
.done:
    and a
    ret
.lost:
    scf
    ret

; --- The game itself: the same on both Game Boys, given the same inputs -----

ResetGame:
    xor a
    ld [wScore1], a
    ld [wScore2], a
    ld [wRedraw], a
    ld a, ST_PLAY
    ld [wState], a
    ld a, PADDLE_Y0
    ld [wP1Y], a
    ld [wP2Y], a
    ld a, SPEED
    ld [wBallDX], a ; the first serve goes to player 2
    ; fall through

; The ball back in the middle, resting before the serve. It leaves up or
; down depending on the score so far.
ResetBall:
    ld a, BALL_X0
    ld [wBallX], a
    ld a, BALL_Y0
    ld [wBallY], a
    ld a, [wScore1]
    ld b, a
    ld a, [wScore2]
    add b
    and 1
    ld a, 1
    jr z, .down
    ld a, -1
.down:
    ld [wBallDY], a
    ld a, SERVE_WAIT
    ld [wServeWait], a
    ret

; One frame of the game, from wIn1 and wIn2.
Step:
    ld hl, wFrame
    inc [hl]
    ld a, [wState]
    cp ST_OVER
    jr nz, .play
    ; Game over: either player's START plays again, SELECT goes to the menu.
    ld a, [wIn1]
    ld b, a
    ld a, [wIn2]
    or b
    bit IN_START, a
    jr nz, .again
    bit IN_SELECT, a
    ret z
    ld a, 1
    ld [wGoTitle], a
    ret
.again:
    call ResetGame
    ld a, 1
    ld [wRedraw], a
    ret

.play:
    ld a, [wIn1]
    ld hl, wP1Y
    call MovePaddle
    ld a, [wIn2]
    ld hl, wP2Y
    call MovePaddle
    ld a, [wServeWait]
    and a
    jr z, .move
    dec a
    ld [wServeWait], a
    ret

.move:
    ld a, [wBallDX]
    ld b, a
    ld a, [wBallX]
    add b
    ld [wBallX], a
    ld a, [wBallDY]
    ld b, a
    ld a, [wBallY]
    add b
    ld [wBallY], a
    ; Bounce off the top and bottom walls.
    cp TOP
    jr nc, .notTop
    ld a, TOP
    ld [wBallY], a
    call NegateDY
    jr .walls
.notTop:
    cp BOTTOM - 8 + 1
    jr c, .walls
    ld a, BOTTOM - 8
    ld [wBallY], a
    call NegateDY
.walls:
    ld a, [wBallDX]
    bit 7, a
    jr z, .rightward

    ; Heading left, towards player 1.
    ld a, [wBallX]
    cp 240
    jr nc, .p2Scores ; went past x = 0 (wrapped)
    cp 2
    jr c, .p2Scores
    cp P1_X + 8 + 1
    ret nc ; not at the paddle yet
    ld a, [wP1Y]
    ld b, a
    call HitsPaddle
    ret nc
    ld [wBallDY], a
    ld a, SPEED
    ld [wBallDX], a
    ld a, P1_X + 8
    ld [wBallX], a
    ret

.rightward:
    ; Heading right, towards player 2.
    ld a, [wBallX]
    cp 156
    jr nc, .p1Scores
    cp P2_X - 8
    ret c ; not at the paddle yet
    ld a, [wP2Y]
    ld b, a
    call HitsPaddle
    ret nc
    ld [wBallDY], a
    ld a, -SPEED
    ld [wBallDX], a
    ld a, P2_X - 8
    ld [wBallX], a
    ret

.p1Scores:
    ld hl, wScore1
    ld b, SPEED ; serve to the player who missed
    jr .point
.p2Scores:
    ld hl, wScore2
    ld b, -SPEED
.point:
    inc [hl]
    ld a, [hl]
    cp WIN_SCORE
    jr nc, .won
    ld a, b
    ld [wBallDX], a
    jp ResetBall
.won:
    ld a, ST_OVER
    ld [wState], a
    ld a, 1
    ld [wRedraw], a
    ret

NegateDY:
    ld a, [wBallDY]
    cpl
    inc a
    ld [wBallDY], a
    ret

; Moves the paddle at [hl] by input a, within the court.
MovePaddle:
    bit IN_UP, a
    jr z, .down
    ld a, [hl]
    sub 2
    cp TOP
    jr nc, .store
    ld a, TOP
    jr .store
.down:
    bit IN_DOWN, a
    ret z
    ld a, [hl]
    add 2
    cp BOTTOM - PADDLE_H + 1
    jr c, .store
    ld a, BOTTOM - PADDLE_H
.store:
    ld [hl], a
    ret

; Does the ball touch the paddle whose top is at b? If so, carry set and a =
; the new vertical speed, steeper the nearer the paddle's ends it hit.
HitsPaddle:
    ld a, [wBallY]
    add 8
    cp b
    jr c, .miss
    jr z, .miss ; ball's bottom at or above the paddle's top
    ld a, b
    add PADDLE_H
    ld c, a
    ld a, [wBallY]
    cp c
    jr nc, .miss ; ball's top at or below the paddle's bottom
    add 4 ; the ball's middle, relative to the paddle's top
    sub b
    cp 128
    jr nc, .steepUp ; above the top
    cp 6
    jr c, .steepUp
    cp 12
    jr c, .up
    cp 18
    jr c, .down
    ld a, 2
    scf
    ret
.steepUp:
    ld a, -2
    scf
    ret
.up:
    ld a, -1
    scf
    ret
.down:
    ld a, 1
    scf
    ret
.miss:
    and a ; carry clear
    ret

; The computer's buttons: follow the ball, at half a player's speed.
CpuInput:
    ld a, [wFrame]
    and 1
    jr z, .think
    xor a
    ret
.think:
    ld a, [wP2Y]
    add PADDLE_H / 2
    ld b, a
    ld a, [wBallY]
    add 4
    sub b
    jr c, .above
    cp 4
    jr c, .stay
    ld a, 1 << IN_DOWN
    ret
.above:
    cp -4
    jr nc, .stay
    ld a, 1 << IN_UP
    ret
.stay:
    xor a
    ret

; --- Input -------------------------------------------------------------------

; Reads the buttons into wMyIn. Returns a = the ones newly pressed.
ReadInput:
    ld a, $20 ; the d-pad
    ldh [rP1], a
    ldh a, [rP1]
    ldh a, [rP1]
    cpl
    and $0F
    ld b, a ; right, left, up, down
    ld a, $10 ; the buttons
    ldh [rP1], a
    ldh a, [rP1]
    ldh a, [rP1]
    cpl
    and $0F
    ld c, a ; A, B, select, start
    ld a, $30
    ldh [rP1], a
    xor a
    bit 2, b
    jr z, .notUp
    set IN_UP, a
.notUp:
    bit 3, b
    jr z, .notDown
    set IN_DOWN, a
.notDown:
    bit 3, c
    jr z, .notStart
    set IN_START, a
.notStart:
    bit 2, c
    jr z, .notSelect
    set IN_SELECT, a
.notSelect:
    bit 1, c
    jr z, .notB
    set IN_B, a
.notB:
    ld b, a
    ld a, [wPrevIn]
    cpl
    and b
    ld c, a
    ld a, b
    ld [wMyIn], a
    ld [wPrevIn], a
    ld a, c
    ret

; --- Screen ------------------------------------------------------------------

; Sprites and scores; call during VBlank.
Draw:
    ld hl, OAM_ADDR
    ld a, [wP1Y]
    add 16
    ld b, a
    ld c, P1_X + 8
    call PaddleSprites
    ld a, [wP2Y]
    add 16
    ld b, a
    ld c, P2_X + 8
    call PaddleSprites
    ld a, [wState]
    cp ST_PLAY
    jr nz, .noBall
    ld a, [wBallY]
    add 16
    ld [hli], a
    ld a, [wBallX]
    add 8
    ld [hli], a
    ld a, T_BALL
    ld [hli], a
    xor a
    ld [hli], a
    jr .scores
.noBall:
    xor a
    ld [hli], a
    ld [hli], a
    ld [hli], a
    ld [hli], a
.scores:
    ld a, [wScore1]
    inc a ; digit n is tile n + 1
    ld [MAP + 7], a
    ld a, [wScore2]
    inc a
    ld [MAP + 12], a
    ret

; Three sprites, one under the other, at y = b, x = c (OAM coordinates).
PaddleSprites:
    ld d, 3
.next:
    ld a, b
    ld [hli], a
    ld a, c
    ld [hli], a
    ld a, T_PADDLE
    ld [hli], a
    xor a
    ld [hli], a
    ld a, b
    add 8
    ld b, a
    dec d
    jr nz, .next
    ret

; Draws the court (and the result, after a game) with the LCD off.
DrawScreen:
    call LcdOff
    call ClearMap
    xor a
    ld [wRedraw], a
    ; The score bar: labels, and a line under it.
    text 1, 0, sP1
    ld a, [wRole]
    cp ROLE_CPU
    jr z, .cpuLabel
    text 17, 0, sP2
    jr .line
.cpuLabel:
    text 16, 0, sCpu
.line:
    ld hl, MAP + 32
    ld b, 20
    ld a, T_BORDER
.border:
    ld [hli], a
    dec b
    jr nz, .border
    ; The net, down the middle.
    ld hl, MAP + 2 * 32 + 9
    ld b, 16
    ld de, 32
.net:
    ld a, T_NET
    ld [hl], a
    add hl, de
    dec b
    jr nz, .net
    ld a, [wState]
    cp ST_OVER
    jr nz, .on
    ; Who won, and what next.
    ld a, [wScore1]
    cp WIN_SCORE
    ld a, [wRole]
    jr c, .p2Won
    cp ROLE_CPU
    jr z, .youWin
    text 3, 7, sP1Wins
    jr .options
.youWin:
    text 6, 7, sYouWin
    jr .options
.p2Won:
    cp ROLE_CPU
    jr z, .cpuWins
    text 3, 7, sP2Wins
    jr .options
.cpuWins:
    text 6, 7, sCpuWins
.options:
    text 4, 9, sAgain
    text 4, 10, sMenu
.on:
    jp LcdOn

; Copies the string at de to the map at hl.
PutText:
    ld a, [de]
    cp END
    ret z
    ld [hli], a
    inc de
    jr PutText

ClearMap:
    ld hl, MAP
    ld bc, 32 * 32
.clear:
    xor a
    ld [hli], a
    dec bc
    ld a, b
    or c
    jr nz, .clear
    ret

HideSprites:
    ld hl, OAM_ADDR
    ld b, 160
    xor a
.clear:
    ld [hli], a
    dec b
    jr nz, .clear
    ret

; Waits for the next VBlank to begin.
WaitVBlank:
.drawing:
    ldh a, [rLY]
    cp 144
    jr nc, .drawing ; still in the last one
.vblank:
    ldh a, [rLY]
    cp 144
    jr c, .vblank
    ret

; Turns the LCD off, at VBlank (turning it off mid-frame can harm a real one).
LcdOff:
    ldh a, [rLCDC]
    bit 7, a
    ret z
.wait:
    ldh a, [rLY]
    cp 144
    jr c, .wait
    xor a
    ldh [rLCDC], a
    ret

LcdOn:
    ld a, LCDC_ON
    ldh [rLCDC], a
    ret

; --- Data --------------------------------------------------------------------

sTitle:    db "LINK PONG", END
sStart2P:  db "START: 2 PLAYERS", END
sVsCpu:    db "SELECT: VS CPU", END
sWaiting:  db "WAITING FOR P2", END
sCancel:   db "B: CANCEL", END
sBlank14:  db "              ", END
sLinkLost: db "LINK LOST", END
sP1:       db "P1", END
sP2:       db "P2", END
sCpu:      db "CPU", END
sP1Wins:   db "PLAYER 1 WINS", END
sP2Wins:   db "PLAYER 2 WINS", END
sYouWin:   db "YOU WIN", END
sCpuWins:  db "CPU WINS", END
sAgain:    db "START: AGAIN", END
sMenu:     db "SELECT: MENU", END

; 8x8 tiles, one bit per pixel, in tile order.
Font:
    ds 8, 0                                ; blank
    db $7C,$C6,$CE,$D6,$E6,$C6,$7C,$00     ; 0
    db $18,$38,$18,$18,$18,$18,$7E,$00     ; 1
    db $7C,$C6,$06,$1C,$70,$C0,$FE,$00     ; 2
    db $7C,$C6,$06,$3C,$06,$C6,$7C,$00     ; 3
    db $1C,$3C,$6C,$CC,$FE,$0C,$0C,$00     ; 4
    db $FE,$C0,$FC,$06,$06,$C6,$7C,$00     ; 5
    db $3C,$60,$C0,$FC,$C6,$C6,$7C,$00     ; 6
    db $FE,$06,$0C,$18,$30,$30,$30,$00     ; 7
    db $7C,$C6,$C6,$7C,$C6,$C6,$7C,$00     ; 8
    db $7C,$C6,$C6,$7E,$06,$0C,$78,$00     ; 9
    db $38,$6C,$C6,$C6,$FE,$C6,$C6,$00     ; A
    db $FC,$C6,$C6,$FC,$C6,$C6,$FC,$00     ; B
    db $7C,$C6,$C0,$C0,$C0,$C6,$7C,$00     ; C
    db $FE,$C0,$C0,$FC,$C0,$C0,$FE,$00     ; E
    db $FE,$C0,$C0,$FC,$C0,$C0,$C0,$00     ; F
    db $7C,$C6,$C0,$CE,$C6,$C6,$7E,$00     ; G
    db $7E,$18,$18,$18,$18,$18,$7E,$00     ; I
    db $C6,$CC,$D8,$F0,$D8,$CC,$C6,$00     ; K
    db $C0,$C0,$C0,$C0,$C0,$C0,$FE,$00     ; L
    db $C6,$EE,$FE,$D6,$C6,$C6,$C6,$00     ; M
    db $C6,$E6,$F6,$DE,$CE,$C6,$C6,$00     ; N
    db $7C,$C6,$C6,$C6,$C6,$C6,$7C,$00     ; O
    db $FC,$C6,$C6,$FC,$C0,$C0,$C0,$00     ; P
    db $FC,$C6,$C6,$FC,$D8,$CC,$C6,$00     ; R
    db $7C,$C6,$C0,$7C,$06,$C6,$7C,$00     ; S
    db $FE,$18,$18,$18,$18,$18,$18,$00     ; T
    db $C6,$C6,$C6,$C6,$C6,$C6,$7C,$00     ; U
    db $C6,$C6,$C6,$C6,$6C,$38,$10,$00     ; V
    db $C6,$C6,$C6,$D6,$FE,$EE,$C6,$00     ; W
    db $C6,$C6,$6C,$38,$18,$18,$18,$00     ; Y
    db $00,$18,$18,$00,$18,$18,$00,$00     ; :
    db $FF,$FF,$FF,$FF,$FF,$FF,$FF,$FF     ; paddle
    db $3C,$7E,$FF,$FF,$FF,$FF,$7E,$3C     ; ball
    db $18,$18,$18,$18,$00,$00,$00,$00     ; net
    db $00,$00,$00,$00,$00,$00,$00,$FF     ; line under the score bar
FontEnd:
