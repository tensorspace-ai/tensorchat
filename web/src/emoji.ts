/**
 * A curated emoji set, for the picker and for `:shortcode:` completion.
 *
 * # Why curated
 *
 * The full Unicode emoji set is some 3,700 characters, and a searchable index
 * of it with keywords is a few hundred kilobytes — several times the size of
 * this entire application. Shipping that to make a reaction bar richer would
 * trade the one property the client is built around.
 *
 * So this is the set people actually reach for in a work chat, at roughly one
 * kilobyte of source. Anything outside it still works: emoji typed or pasted
 * directly are ordinary text, and the reaction API takes whatever character you
 * send it. What you lose is only the ability to *find* those by name here.
 *
 * # Format
 *
 * One entry per line, space-separated: the character, its canonical shortcode,
 * then any extra search terms. Written as a single string and parsed on first
 * use, because 300 array literals cost far more bytes than 300 lines do.
 */

export type Emoji = {
  char: string;
  /** Canonical name, without colons. What `:shortcode:` completes to. */
  name: string;
  /** Everything the entry matches on, including the name. */
  terms: string[];
  category: string;
};

const TABLE: Array<[string, string]> = [
  [
    'Smileys',
    `😀 grinning grin happy
😃 smiley happy joy
😄 smile happy joy laugh
😁 grin happy
😆 laughing lol haha
😂 joy lol crying laugh tears
🤣 rofl rolling laughing
🙂 slightly_smiling_face smile
😉 wink
😊 blush smile happy
😍 heart_eyes love
😘 kissing_heart kiss
🤗 hugs hug
🤔 thinking think hmm
🤨 raised_eyebrow skeptical
😐 neutral_face meh
😑 expressionless
🙄 roll_eyes eyeroll
😏 smirk
😴 sleeping sleep zzz
😪 sleepy tired
😌 relieved
🤤 drooling
😜 stuck_out_tongue_winking_eye silly
🤪 zany crazy
🤓 nerd glasses
😎 sunglasses cool
🥳 partying celebrate party
😕 confused
😟 worried
🙁 slightly_frowning_face sad
😬 grimacing awkward yikes
😰 anxious sweat nervous
😢 cry sad tear
😭 sob crying sad
😤 triumph huff
😠 angry mad
😡 rage furious mad
🤯 exploding_head mindblown wow
😳 flushed embarrassed
🥵 hot heat
🥶 cold freezing
😱 scream fear shocked
😨 fearful scared
🤩 star_struck amazed wow
🥺 pleading puppy please
😇 innocent halo
🤠 cowboy
🤫 shushing quiet shh
🤐 zipper_mouth silence
🤢 nauseated sick
🤮 vomiting sick
🤧 sneezing sick
😷 mask sick
🤒 thermometer_face sick ill
🤕 head_bandage hurt
🥴 woozy drunk
😵 dizzy_face knocked_out
🤑 money_mouth rich
🤥 lying liar pinocchio
🫠 melting embarrassed
💀 skull dead rip
👻 ghost boo
👽 alien
🤖 robot bot
💩 poop crap
🔥 fire lit hot flame
⭐ star
✨ sparkles shiny magic
💫 dizzy
💥 boom explosion
💯 100 hundred perfect
❤️ heart love red_heart
🧡 orange_heart
💛 yellow_heart
💚 green_heart
💙 blue_heart
💜 purple_heart
🖤 black_heart
💔 broken_heart
🎉 tada party celebrate hooray
🎊 confetti_ball celebrate
🎁 gift present
🎂 birthday cake`,
  ],
  [
    'Gestures',
    `👍 thumbsup +1 yes good approve
👎 thumbsdown -1 no bad
👌 ok_hand perfect
🤌 pinched_fingers italian
✌️ v peace victory
🤞 crossed_fingers luck hope
🤟 love_you_gesture
🤘 metal rock
🤙 call_me shaka
👈 point_left
👉 point_right
👆 point_up
👇 point_down
☝️ point_up_2 one
✋ raised_hand stop
🤚 raised_back_of_hand
🖐️ hand fingers_splayed
🖖 vulcan spock
👋 wave hello hi bye
🤝 handshake deal agree
🙏 pray thanks please namaste
✍️ writing_hand
💪 muscle strong flex
🦾 mechanical_arm
🖕 middle_finger
👏 clap applause bravo
🙌 raised_hands praise celebrate
👐 open_hands
🤲 palms_up
🫡 salute yes_sir
🫠 melted
🙋 raising_hand question volunteer
🤦 facepalm
🤷 shrug dunno idk
💁 tipping_hand information
🙇 bow sorry
👀 eyes look watching seen
👁️ eye
🧠 brain smart`,
  ],
  [
    'Objects',
    `💻 computer laptop
🖥️ desktop_computer
⌨️ keyboard
🖱️ computer_mouse pointer
📱 iphone phone mobile
☎️ telephone
📞 telephone_receiver call
📠 fax
🔋 battery
🔌 electric_plug
💾 floppy_disk save
💿 cd disc
📀 dvd
🗄️ file_cabinet
📁 file_folder folder
📂 open_file_folder
📄 page_facing_up document file
📃 page_with_curl
📊 bar_chart chart graph
📈 chart_increasing up growth
📉 chart_decreasing down loss
📋 clipboard
📌 pushpin pin
📍 round_pushpin location
🔖 bookmark
🏷️ label tag
📎 paperclip attachment
🔗 link
✏️ pencil write edit
🖊️ pen
🔍 mag search magnifier
🔎 mag_right search
🔒 lock secure private
🔓 unlock
🔑 key
🗝️ old_key
🔨 hammer build fix
🛠️ hammer_and_wrench tools
⚙️ gear settings config
🧰 toolbox
🧪 test_tube experiment
🧫 petri_dish
🔬 microscope research
🔭 telescope
📡 satellite_antenna
💡 bulb idea light
🔦 flashlight
🕯️ candle
🗑️ wastebasket trash delete bin
📦 package box shipping
📮 postbox
✉️ envelope mail email
📧 e-mail email
📨 incoming_envelope
📬 mailbox
🗓️ calendar date
⏰ alarm_clock
⏱️ stopwatch timer
⏳ hourglass_flowing_sand waiting
⌛ hourglass time
🕐 clock1 time
💰 moneybag money
💵 dollar money
💳 credit_card payment
🧾 receipt invoice
⚖️ balance_scale justice
🎯 dart target goal bullseye
🎲 game_die dice random
🧩 puzzle_piece
🎮 video_game gaming
🏆 trophy win first
🥇 first_place gold
🥈 second_place silver
🥉 third_place bronze
🎖️ medal
🚀 rocket ship launch deploy
🛸 flying_saucer ufo
🛰️ satellite
✈️ airplane flight
🚗 car
🚕 taxi
🚌 bus
🚲 bicycle bike
🏠 house home
🏢 office building work
🏭 factory
🌍 earth_africa world globe
🗺️ world_map map`,
  ],
  [
    'Nature & food',
    `🐶 dog puppy
🐱 cat kitten
🐭 mouse
🐹 hamster
🐰 rabbit bunny
🦊 fox
🐻 bear
🐼 panda
🐨 koala
🐯 tiger
🦁 lion
🐮 cow
🐷 pig
🐸 frog
🐵 monkey
🐔 chicken
🐧 penguin
🐦 bird
🦆 duck
🦅 eagle
🦉 owl
🦇 bat
🐺 wolf
🐗 boar
🐴 horse
🦄 unicorn
🐝 bee
🐛 bug insect
🦋 butterfly
🐌 snail slow
🐞 lady_beetle ladybug
🐜 ant
🕷️ spider
🐢 turtle slow
🐍 snake
🦎 lizard
🐙 octopus
🦑 squid
🦐 shrimp
🦀 crab
🐡 blowfish
🐠 tropical_fish
🐟 fish
🐬 dolphin
🐳 whale
🦈 shark
🌵 cactus
🎄 christmas_tree
🌲 evergreen_tree tree
🌳 deciduous_tree tree
🌴 palm_tree
🌱 seedling sprout growth
🌿 herb plant
🍀 four_leaf_clover luck
🍁 maple_leaf
🌸 cherry_blossom flower
🌹 rose flower
🌻 sunflower
🌼 blossom flower
🌷 tulip
🌞 sun_with_face sunny
☀️ sunny sun clear
🌤️ partly_sunny
☁️ cloud cloudy
🌧️ rain raining
⛈️ thunderstorm storm
❄️ snowflake snow cold
⛄ snowman
🌈 rainbow
🌊 ocean wave water
🌙 crescent_moon night
🍎 apple
🍊 tangerine orange
🍋 lemon
🍌 banana
🍉 watermelon
🍇 grapes
🍓 strawberry
🥝 kiwi
🍅 tomato
🥑 avocado
🌽 corn
🥕 carrot
🥐 croissant
🍞 bread
🧀 cheese
🥚 egg
🍳 fried_egg cooking
🥞 pancakes
🥓 bacon
🍔 hamburger burger
🍟 fries
🍕 pizza
🌭 hotdog
🌮 taco
🌯 burrito
🥗 salad
🍜 ramen noodles
🍣 sushi
🍦 ice_cream
🍩 doughnut donut
🍪 cookie
🍫 chocolate_bar
🍿 popcorn
☕ coffee tea
🍵 tea
🍺 beer
🍻 beers cheers
🥂 clinking_glasses cheers toast
🍷 wine
🥃 whisky
🍸 cocktail
🧊 ice cube`,
  ],
  [
    'Symbols',
    `✅ white_check_mark check done yes
☑️ ballot_box_with_check
✔️ heavy_check_mark check
❌ x cross no wrong
❎ negative_squared_cross_mark
⛔ no_entry stop
🚫 prohibited forbidden
⚠️ warning caution
❗ exclamation important
❓ question
❔ grey_question
‼️ bangbang
⁉️ interrobang
💤 zzz sleep
🔴 red_circle
🟠 orange_circle
🟡 yellow_circle
🟢 green_circle
🔵 blue_circle
🟣 purple_circle
⚫ black_circle
⚪ white_circle
🟥 red_square
🟩 green_square
🟦 blue_square
🔶 large_orange_diamond
🔷 large_blue_diamond
▶️ arrow_forward play
⏸️ pause_button pause
⏹️ stop_button stop
⏺️ record_button record
⏭️ next_track skip
🔁 repeat loop
🔀 shuffle random
🔄 arrows_counterclockwise refresh retry sync
⬆️ arrow_up
⬇️ arrow_down
⬅️ arrow_left
➡️ arrow_right
↩️ leftwards_arrow_with_hook reply
🔝 top
🆕 new
🆗 ok
🆙 up
🔜 soon
🚧 construction wip
🏁 checkered_flag finish done
🚩 triangular_flag
🏳️ white_flag surrender
♻️ recycle
🔕 no_bell muted mute
🔔 bell notification
📢 loudspeaker announce
📣 mega megaphone shout
💬 speech_balloon comment chat
💭 thought_balloon
🗯️ anger_bubble
👤 bust_in_silhouette user person
👥 busts_in_silhouette users people team
🔐 closed_lock_with_key
🛡️ shield security
⚡ zap lightning fast
🌟 star2 glowing_star`,
  ],
];

let parsed: Emoji[] | null = null;

/** The whole set, parsed on first use. */
export function allEmoji(): Emoji[] {
  if (parsed) return parsed;
  const out: Emoji[] = [];
  for (const [category, block] of TABLE) {
    for (const line of block.split('\n')) {
      const parts = line.trim().split(' ');
      if (parts.length < 2) continue;
      const [char, name, ...rest] = parts;
      out.push({ char, name, terms: [name, ...rest], category });
    }
  }
  parsed = out;
  return out;
}

/** The categories, in display order. */
export function categories(): string[] {
  return TABLE.map(([name]) => name);
}

/**
 * Search by shortcode or keyword.
 *
 * Prefix matches on the canonical name rank above keyword matches, so typing
 * `:th` offers `thumbsup` before `earth_africa` — the name you were most likely
 * spelling out.
 */
export function searchEmoji(query: string, limit = 24): Emoji[] {
  const q = query.trim().toLowerCase().replace(/^:/, '');
  if (!q) return allEmoji().slice(0, limit);

  const exact: Emoji[] = [];
  const prefix: Emoji[] = [];
  const loose: Emoji[] = [];
  for (const e of allEmoji()) {
    if (e.name === q) exact.push(e);
    else if (e.name.startsWith(q)) prefix.push(e);
    else if (e.terms.some((t) => t.includes(q))) loose.push(e);
  }
  return [...exact, ...prefix, ...loose].slice(0, limit);
}

/** Look up one emoji by its exact shortcode. */
export function emojiByName(name: string): Emoji | undefined {
  const want = name.replace(/^:|:$/g, '').toLowerCase();
  return allEmoji().find((e) => e.name === want);
}

/**
 * Replace `:shortcode:` runs in a string with their characters.
 *
 * Applied when a message is sent rather than when it is rendered, so what is
 * stored is the emoji itself. A receiver — including a bot, or a client that
 * never had this table — then needs no shortcode knowledge at all.
 */
export function expandShortcodes(text: string): string {
  return text.replace(/:([a-z0-9_+-]+):/gi, (whole, name: string) => {
    const found = emojiByName(name);
    return found ? found.char : whole;
  });
}
