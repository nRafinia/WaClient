//! Embedded emoji dataset, search, recents and reaction aggregation.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const QUICK_REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];
pub const MAX_RECENTS: usize = 24;
pub const MAX_SEARCH_RESULTS: usize = 120;

#[derive(Debug, PartialEq, Eq)]
pub struct Emoji {
    pub ch: &'static str,
    pub keywords: &'static str,
}

#[derive(Debug)]
pub struct Category {
    pub name: &'static str,
    /// Emoji shown on the category tab.
    pub icon: &'static str,
    pub emoji: Vec<Emoji>,
}

// One emoji per line: "<emoji> <keywords...>".
const SMILEYS: &str = "\
😀 grinning smile happy
😃 smiley happy joy
😄 smile happy laugh
😁 grin beaming teeth
😆 laughing satisfied squint
😅 sweat nervous laugh relief
🤣 rofl rolling floor laughing lol
😂 joy tears laugh lol
🙂 slight smile
🙃 upside down silly
😉 wink
😊 blush happy shy
😇 angel halo innocent
🥰 love hearts adore
😍 heart eyes love
🤩 star struck wow
😘 kiss blow love
😗 kissing
😚 kissing closed eyes
😙 kissing smiling
😋 yum delicious tasty
😛 tongue
😜 wink tongue crazy
🤪 zany crazy wild
😝 squint tongue
🤑 money mouth rich
🤗 hug hugging
🤭 giggle oops hand over mouth
🤫 shush quiet secret
🤔 thinking hmm
🤐 zipper mouth secret
🤨 raised eyebrow skeptical
😐 neutral meh
😑 expressionless blank
😶 no mouth speechless
😏 smirk
😒 unamused annoyed
🙄 eye roll
😬 grimace awkward
😮‍💨 exhale sigh relief
🤥 lying pinocchio
😌 relieved calm
😔 pensive sad
😪 sleepy tired
🤤 drooling
😴 sleeping zzz sleep
😷 mask sick
🤒 thermometer sick fever
🤕 bandage hurt injured
🤢 nauseated sick green
🤮 vomit sick
🤧 sneeze achoo
🥵 hot overheated
🥶 cold freezing
🥴 woozy dizzy drunk
😵 dizzy knocked out
🤯 exploding head mind blown
🤠 cowboy hat
🥳 party celebrate birthday
🥸 disguise glasses
😎 cool sunglasses
🤓 nerd glasses
🧐 monocle curious
😕 confused
😟 worried
🙁 frown sad
☹️ frowning sad
😮 open mouth surprised wow
😯 hushed surprised
😲 astonished shocked
😳 flushed embarrassed
🥺 pleading puppy eyes please
😦 frowning open mouth
😧 anguished
😨 fearful scared
😰 anxious sweat
😥 sad relieved
😢 cry sad tear
😭 sob crying loud
😱 scream fear shocked
😖 confounded
😣 persevere
😞 disappointed
😓 downcast sweat
😩 weary tired
😫 tired exhausted
🥱 yawn bored tired
😤 triumph huff angry
😡 angry rage red mad
😠 angry mad
🤬 cursing swearing angry
😈 devil smiling horns
👿 imp angry devil
💀 skull dead
☠️ skull crossbones
💩 poop
🤡 clown
👹 ogre monster
👻 ghost halloween
👽 alien
🤖 robot
😺 cat smile
😸 cat grin
😹 cat joy tears
😻 cat heart eyes
🙈 see no evil monkey
🙉 hear no evil monkey
🙊 speak no evil monkey
💋 kiss lips
💌 love letter
💘 heart arrow cupid
💝 heart ribbon gift
💖 sparkling heart
💗 growing heart
💓 beating heart
💞 revolving hearts
💕 two hearts
💔 broken heart
❤️‍🔥 heart on fire
❤️ red heart love
🧡 orange heart
💛 yellow heart
💚 green heart
💙 blue heart
💜 purple heart
🖤 black heart
🤍 white heart
🤎 brown heart
💯 hundred perfect score
💢 anger symbol
💥 collision boom bang
💫 dizzy star
💦 sweat droplets water
💨 dash wind fast
💬 speech bubble chat message
💭 thought bubble
💤 zzz sleep
👋 wave hello hi bye
🤚 raised back hand
✋ raised hand stop high five
🖖 vulcan salute
👌 ok perfect
🤌 pinched fingers italian
✌️ victory peace
🤞 crossed fingers luck
🤟 love you gesture
🤘 rock on horns
🤙 call me shaka
👈 point left
👉 point right
👆 point up
👇 point down
☝️ index up
👍 thumbs up like yes good
👎 thumbs down dislike no bad
✊ fist raised
👊 fist bump punch
👏 clap applause bravo
🙌 raising hands celebrate hooray
👐 open hands
🤲 palms up together
🤝 handshake deal
🙏 pray thanks please folded hands
✍️ writing hand
💅 nail polish
🤳 selfie
💪 muscle strong flex biceps
🦵 leg
🦶 foot
👂 ear listen
👃 nose
🧠 brain smart
👀 eyes look watching
👁️ eye
👅 tongue
👄 mouth lips
👶 baby
🧒 child
👦 boy
👧 girl
🧑 person adult
👨 man
👩 woman
🧓 older person
👴 old man
👵 old woman
🙍 frowning person
🙎 pouting person
🙅 no gesture
🙆 ok gesture
💁 information desk person
🙋 raising hand
🧏 deaf person
🙇 bow sorry respect
🤦 facepalm
🤷 shrug dunno
👮 police officer
🕵️ detective
💂 guard
🧑‍⚕️ doctor health worker
🧑‍🎓 student graduate
🧑‍🏫 teacher
🧑‍💻 technologist developer programmer
🧑‍🍳 cook chef
🧑‍🚀 astronaut
🦸 superhero
🧙 mage wizard
🧛 vampire
🧜 merperson mermaid
🧚 fairy
🧞 genie
🧟 zombie
💃 dancer dance woman
🕺 man dancing dance
🏃 running run
🚶 walking walk
🧘 yoga lotus meditation
👪 family
👫 couple holding hands
💑 couple heart
💏 kiss couple
🗣️ speaking head talk
👤 silhouette user
👥 silhouettes users group";

const ANIMALS: &str = "\
🐶 dog puppy pet
🐱 cat kitten pet
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
🐽 pig nose
🐸 frog
🐵 monkey
🐒 monkey
🦍 gorilla
🐔 chicken
🐧 penguin
🐦 bird
🐤 baby chick
🦆 duck
🦅 eagle
🦉 owl
🦇 bat
🐺 wolf
🐗 boar
🐴 horse
🦄 unicorn
🐝 bee honeybee
🐛 bug caterpillar
🦋 butterfly
🐌 snail
🐞 ladybug
🐜 ant
🦟 mosquito
🪲 beetle
🕷️ spider
🕸️ spider web
🦂 scorpion
🐢 turtle
🐍 snake
🦎 lizard
🦖 t-rex dinosaur
🦕 sauropod dinosaur
🐙 octopus
🦑 squid
🦐 shrimp
🦞 lobster
🦀 crab
🐡 blowfish pufferfish
🐠 tropical fish
🐟 fish
🐬 dolphin
🐳 whale
🐋 whale
🦈 shark
🐊 crocodile
🐅 tiger
🐆 leopard
🦓 zebra
🐘 elephant
🦛 hippo
🦏 rhino
🐪 camel
🦒 giraffe
🦘 kangaroo
🐃 water buffalo
🐂 ox
🐄 cow
🐎 horse racing
🐖 pig
🐏 ram
🐑 sheep
🦙 llama
🐐 goat
🦌 deer
🐕 dog
🐩 poodle
🐈 cat
🐓 rooster
🦃 turkey
🦚 peacock
🦜 parrot
🦢 swan
🕊️ dove peace
🐇 rabbit
🦝 raccoon
🦨 skunk
🦡 badger
🦦 otter
🦥 sloth
🐁 mouse
🐀 rat
🐿️ chipmunk squirrel
🦔 hedgehog
🐾 paw prints
🐉 dragon
🐲 dragon face
🌵 cactus
🎄 christmas tree
🌲 evergreen tree
🌳 tree
🌴 palm tree
🌱 seedling sprout
🌿 herb leaf
☘️ shamrock clover
🍀 four leaf clover luck
🎍 bamboo
🍃 leaf wind
🍂 fallen leaf autumn
🍁 maple leaf autumn
🍄 mushroom
🐚 shell
🌾 rice sheaf
💐 bouquet flowers
🌷 tulip
🌹 rose flower
🥀 wilted flower
🌺 hibiscus
🌸 cherry blossom
🌼 blossom daisy
🌻 sunflower
🌞 sun face
🌝 full moon face
🌛 moon face
🌙 crescent moon night
🌍 earth globe europe africa
🌎 earth globe americas
🌏 earth globe asia
🪐 planet saturn
⭐ star
🌟 glowing star
✨ sparkles shine
⚡ lightning bolt zap
☄️ comet
🔥 fire hot lit
🌈 rainbow
☀️ sun sunny
⛅ partly sunny cloud
☁️ cloud
🌧️ rain cloud
⛈️ thunderstorm
🌩️ lightning cloud
❄️ snowflake cold
☃️ snowman
⛄ snowman
🌬️ wind
💧 droplet water
🌊 wave ocean sea";

const FOOD: &str = "\
🍏 green apple
🍎 red apple
🍐 pear
🍊 orange tangerine
🍋 lemon
🍌 banana
🍉 watermelon
🍇 grapes
🍓 strawberry
🫐 blueberries
🍈 melon
🍒 cherries
🍑 peach
🥭 mango
🍍 pineapple
🥥 coconut
🥝 kiwi
🍅 tomato
🍆 eggplant aubergine
🥑 avocado
🥦 broccoli
🥬 leafy green lettuce
🥒 cucumber
🌶️ hot pepper chili
🌽 corn
🥕 carrot
🧄 garlic
🧅 onion
🥔 potato
🍠 sweet potato
🥐 croissant
🥯 bagel
🍞 bread
🥖 baguette
🧀 cheese
🥚 egg
🍳 cooking fried egg
🥞 pancakes
🧇 waffle
🥓 bacon
🥩 steak meat
🍗 poultry leg chicken
🍖 meat bone
🌭 hot dog
🍔 hamburger burger
🍟 fries
🍕 pizza
🥪 sandwich
🌮 taco
🌯 burrito
🥙 stuffed flatbread falafel
🧆 falafel
🥗 salad
🍝 spaghetti pasta
🍜 ramen noodles
🍲 stew pot
🍛 curry rice
🍣 sushi
🍤 fried shrimp tempura
🍙 rice ball
🍚 rice
🍥 fish cake
🥟 dumpling
🍢 oden
🍡 dango
🍧 shaved ice
🍨 ice cream
🍦 soft ice cream
🥧 pie
🧁 cupcake
🍰 cake shortcake
🎂 birthday cake
🍮 custard pudding
🍭 lollipop
🍬 candy sweet
🍫 chocolate
🍿 popcorn
🍩 donut
🍪 cookie
🌰 chestnut
🥜 peanuts
🍯 honey
🥛 milk glass
🍼 baby bottle
☕ coffee tea hot beverage
🍵 tea matcha
🧃 juice box
🥤 cup straw soda
🧋 bubble tea boba
🍶 sake
🍺 beer
🍻 cheers beers
🥂 clinking glasses champagne toast
🍷 wine
🥃 whisky tumbler
🍸 cocktail
🍹 tropical drink
🍾 champagne bottle
🧊 ice
🥄 spoon
🍴 fork knife cutlery
🍽️ plate dinner
🥢 chopsticks";

const ACTIVITY: &str = "\
⚽ soccer football
🏀 basketball
🏈 american football
⚾ baseball
🥎 softball
🎾 tennis
🏐 volleyball
🏉 rugby
🎱 pool billiards 8 ball
🏓 ping pong table tennis
🏸 badminton
🥅 goal net
🏒 ice hockey
⛳ golf flag
🏹 bow arrow archery
🎣 fishing
🥊 boxing glove
🥋 martial arts
🎽 running shirt
🛹 skateboard
⛸️ ice skate
🎿 ski
🏂 snowboarder
🏋️ weight lifting gym
🤸 cartwheel gymnastics
🤺 fencing
🏇 horse racing
🏊 swimming swim
🚴 cycling bike
🧗 climbing
🏆 trophy winner
🥇 gold medal first
🥈 silver medal second
🥉 bronze medal third
🏅 sports medal
🎖️ military medal
🎗️ reminder ribbon
🎫 ticket
🎟️ admission tickets
🎪 circus tent
🎭 performing arts theater
🎨 art palette painting
🎬 clapper board movie
🎤 microphone karaoke
🎧 headphones music
🎼 musical score
🎹 piano keyboard
🥁 drum
🎷 saxophone
🎺 trumpet
🎸 guitar
🎻 violin
🎲 dice game
♟️ chess pawn
🎯 direct hit target dart
🎳 bowling
🎮 video game controller
🎰 slot machine
🧩 puzzle piece
🎉 party popper tada celebration
🎊 confetti ball
🎈 balloon
🎁 gift present
🎀 ribbon bow
🎃 jack o lantern halloween
🎆 fireworks
🎇 sparkler";

const TRAVEL: &str = "\
🚗 car automobile
🚕 taxi
🚙 suv
🚌 bus
🚎 trolleybus
🏎️ racing car
🚓 police car
🚑 ambulance
🚒 fire engine
🚚 delivery truck
🚛 articulated lorry
🚜 tractor
🏍️ motorcycle
🛵 scooter
🚲 bicycle bike
🛴 kick scooter
🚨 police light siren
🚥 traffic light
🚧 construction
⚓ anchor
⛵ sailboat
🚤 speedboat
🛳️ passenger ship
⛴️ ferry
🚢 ship
✈️ airplane plane flight
🛫 takeoff departure
🛬 landing arrival
🚁 helicopter
🚀 rocket launch
🛸 flying saucer ufo
🚂 locomotive train
🚆 train
🚄 high speed train
🚇 metro subway
🚊 tram
🚉 station
🏠 house home
🏡 house garden
🏢 office building
🏥 hospital
🏦 bank
🏨 hotel
🏪 convenience store
🏫 school
🏭 factory
🏰 castle
🏯 japanese castle
🗼 tokyo tower
🗽 statue of liberty
⛪ church
🕌 mosque
🕍 synagogue
⛩️ shinto shrine
🕋 kaaba
⛲ fountain
⛺ tent camping
🌁 foggy
🌃 night stars
🏙️ cityscape
🌄 sunrise mountains
🌅 sunrise
🌆 dusk city
🌇 sunset
🌉 bridge night
🎠 carousel horse
🎡 ferris wheel
🎢 roller coaster
🗺️ world map
🧭 compass
🏔️ snow capped mountain
⛰️ mountain
🌋 volcano
🏕️ camping
🏖️ beach umbrella
🏝️ desert island
🗿 moai
🧳 luggage
⌛ hourglass
⏰ alarm clock
🕐 one oclock clock
⏱️ stopwatch";

const OBJECTS: &str = "\
⌚ watch
📱 phone mobile smartphone
📲 phone arrow call
☎️ telephone
📞 telephone receiver
📟 pager
📠 fax
🔋 battery
🔌 electric plug
💻 laptop computer
🖥️ desktop computer
🖨️ printer
⌨️ keyboard
🖱️ mouse computer
💽 minidisc
💾 floppy disk save
💿 cd disc
📷 camera photo
📸 camera flash
📹 video camera
🎥 movie camera
📺 television tv
📻 radio
🎙️ studio microphone podcast
⏳ hourglass flowing
📡 satellite antenna
💡 light bulb idea
🔦 flashlight
🕯️ candle
🧯 fire extinguisher
💸 money wings
💵 dollar cash banknote
💴 yen banknote
💶 euro banknote
💷 pound banknote
💰 money bag
💳 credit card
💎 gem diamond
⚖️ balance scale
🧰 toolbox
🔧 wrench tool
🔨 hammer
⚒️ hammer pick
🛠️ hammer wrench tools
⛏️ pick
🔩 nut bolt
⚙️ gear settings
🧲 magnet
🔫 water pistol gun
💣 bomb
🧨 firecracker
🔪 knife
🗡️ dagger sword
🛡️ shield
🚬 cigarette smoking
⚰️ coffin
🔮 crystal ball
📿 prayer beads
💈 barber pole
🔭 telescope
🔬 microscope
💊 pill medicine
💉 syringe vaccine
🩸 blood drop
🧬 dna
🦠 microbe virus germ
🧪 test tube
🌡️ thermometer
🧹 broom
🧺 basket laundry
🧻 toilet paper
🚽 toilet
🚿 shower
🛁 bathtub
🧼 soap
🧽 sponge
🔑 key
🗝️ old key
🚪 door
🛋️ couch lamp
🛏️ bed
🧸 teddy bear
🖼️ framed picture
🛍️ shopping bags
🛒 shopping cart
✉️ envelope mail
📧 email
📦 package box parcel
📫 mailbox
📝 memo note write
📁 folder
📂 open folder
📅 calendar date
📆 tear off calendar
📈 chart increasing up
📉 chart decreasing down
📊 bar chart
📋 clipboard
📌 pushpin pin
📍 round pushpin location
📎 paperclip
🖇️ linked paperclips
📏 ruler
📐 triangular ruler
✂️ scissors
🗃️ card file box
🗑️ wastebasket trash
🔒 locked lock
🔓 unlocked
🔏 locked pen
🔐 locked key
📖 open book read
📚 books
📓 notebook
📒 ledger
📰 newspaper
🏷️ label tag
🔖 bookmark
🔍 magnifying glass search
🔎 magnifying glass right search
✏️ pencil
🖊️ pen
🖌️ paintbrush
🖍️ crayon";

const SYMBOLS: &str = "\
✅ check mark button done yes
✔️ check mark
☑️ ballot box check
❌ cross mark no wrong
❎ cross mark button
➕ plus add
➖ minus
➗ divide
✖️ multiply
♾️ infinity
‼️ double exclamation
⁉️ exclamation question
❓ question mark red
❔ question mark white
❕ exclamation white
❗ exclamation red mark
〰️ wavy dash
💲 dollar sign
⚠️ warning caution
🚫 prohibited forbidden no
⛔ no entry
🔞 no one under eighteen adult
📵 no mobile phones
🚭 no smoking
♻️ recycling recycle
⚜️ fleur de lis
🔱 trident
〽️ part alternation mark
🔰 beginner
⭕ hollow red circle
🔴 red circle
🟠 orange circle
🟡 yellow circle
🟢 green circle
🔵 blue circle
🟣 purple circle
⚫ black circle
⚪ white circle
🟥 red square
🟧 orange square
🟨 yellow square
🟩 green square
🟦 blue square
🟪 purple square
⬛ black square
⬜ white square
🔶 large orange diamond
🔷 large blue diamond
🔺 red triangle up
🔻 red triangle down
💠 diamond with dot
🔘 radio button
⬆️ up arrow
⬇️ down arrow
⬅️ left arrow
➡️ right arrow
↗️ up right arrow
↘️ down right arrow
↙️ down left arrow
↖️ up left arrow
↕️ up down arrow
↔️ left right arrow
↩️ return left arrow
↪️ right arrow curving
🔃 clockwise arrows
🔄 counterclockwise arrows refresh
🔙 back arrow
🔚 end arrow
🔛 on arrow
🔜 soon arrow
🔝 top arrow
▶️ play button
⏸️ pause button
⏯️ play pause
⏹️ stop button
⏺️ record button
⏭️ next track
⏮️ previous track
⏩ fast forward
⏪ rewind
🔀 shuffle
🔁 repeat
🔼 up button
🔽 down button
🎦 cinema
🔅 dim
🔆 bright
📶 signal bars
📳 vibration mode
📴 mobile phone off
🔇 muted speaker
🔈 speaker low
🔉 speaker medium
🔊 speaker high volume
🔔 bell notification
🔕 bell slash mute
📣 megaphone
📢 loudspeaker
♈ aries
♉ taurus
♊ gemini
♋ cancer
♌ leo
♍ virgo
♎ libra
♏ scorpio
♐ sagittarius
♑ capricorn
♒ aquarius
♓ pisces
⛎ ophiuchus
🆗 ok button
🆙 up button
🆒 cool button
🆕 new button
🆓 free button
🆘 sos
🅰️ a button blood type
🅱️ b button blood type
🆎 ab button
🅾️ o button
ℹ️ information
Ⓜ️ circled m
🅿️ parking
🆚 versus vs
🔠 input latin uppercase
🔤 input latin letters
0️⃣ keycap zero
1️⃣ keycap one
2️⃣ keycap two
3️⃣ keycap three
4️⃣ keycap four
5️⃣ keycap five
6️⃣ keycap six
7️⃣ keycap seven
8️⃣ keycap eight
9️⃣ keycap nine
🔟 keycap ten
#️⃣ keycap hash number
*️⃣ keycap asterisk
©️ copyright
®️ registered
™️ trade mark
☮️ peace symbol
☯️ yin yang
✝️ cross
☪️ star and crescent
🕉️ om
✡️ star of david
☢️ radioactive
☣️ biohazard
💟 heart decoration
❣️ heart exclamation
❤️‍🩹 mending heart";

const FLAGS: &str = "\
🏁 chequered flag finish race
🚩 triangular flag red flag
🎌 crossed flags
🏴 black flag
🏳️ white flag surrender
🏳️‍🌈 rainbow flag pride
🏴‍☠️ pirate flag
🇺🇸 flag united states usa america
🇬🇧 flag united kingdom uk britain
🇨🇦 flag canada
🇦🇺 flag australia
🇩🇪 flag germany
🇫🇷 flag france
🇪🇸 flag spain
🇮🇹 flag italy
🇵🇹 flag portugal
🇳🇱 flag netherlands
🇧🇷 flag brazil
🇲🇽 flag mexico
🇦🇷 flag argentina
🇮🇳 flag india
🇨🇳 flag china
🇯🇵 flag japan
🇰🇷 flag south korea
🇷🇺 flag russia
🇺🇦 flag ukraine
🇹🇷 flag turkey
🇸🇦 flag saudi arabia
🇦🇪 flag united arab emirates uae
🇪🇬 flag egypt
🇮🇷 flag iran
🇮🇶 flag iraq
🇮🇱 flag israel
🇵🇸 flag palestine
🇿🇦 flag south africa
🇳🇬 flag nigeria
🇰🇪 flag kenya
🇵🇰 flag pakistan
🇧🇩 flag bangladesh
🇮🇩 flag indonesia
🇵🇭 flag philippines
🇸🇪 flag sweden
🇳🇴 flag norway
🇨🇭 flag switzerland
🇬🇷 flag greece
🇵🇱 flag poland
🇪🇺 flag european union";

const TABLE: [(&str, &str, &str); 8] = [
    ("Smileys & People", "😀", SMILEYS),
    ("Animals & Nature", "🐻", ANIMALS),
    ("Food & Drink", "🍔", FOOD),
    ("Activity", "⚽", ACTIVITY),
    ("Travel & Places", "🚗", TRAVEL),
    ("Objects", "💡", OBJECTS),
    ("Symbols", "❤️", SYMBOLS),
    ("Flags", "🏁", FLAGS),
];

pub fn categories() -> &'static [Category] {
    static CATS: OnceLock<Vec<Category>> = OnceLock::new();
    CATS.get_or_init(|| {
        TABLE
            .iter()
            .map(|(name, icon, data)| Category {
                name,
                icon,
                emoji: data
                    .lines()
                    .filter_map(|l| {
                        let (ch, keywords) = l.split_once(' ')?;
                        Some(Emoji { ch, keywords })
                    })
                    .collect(),
            })
            .collect()
    })
}

/// Case-insensitive keyword search. Keyword-prefix matches rank before plain
/// substring matches; each emoji appears once.
pub fn search(query: &str) -> Vec<&'static Emoji> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut prefix = Vec::new();
    let mut substring = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for e in categories().iter().flat_map(|c| c.emoji.iter()) {
        if !seen.insert(e.ch) {
            continue;
        }
        if e.ch == q || e.keywords.split(' ').any(|w| w.starts_with(&q)) {
            prefix.push(e);
        } else if e.keywords.contains(&q) {
            substring.push(e);
        }
    }
    prefix.extend(substring);
    prefix.truncate(MAX_SEARCH_RESULTS);
    prefix
}

/// Most-recently-used emoji, persisted as a JSON array of strings.
#[derive(Debug, Default)]
pub struct Recents {
    items: Vec<String>,
    path: Option<PathBuf>,
}

impl Recents {
    /// Missing or corrupt files yield an empty list.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut items: Vec<String> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        items.truncate(MAX_RECENTS);
        Self { items, path: Some(path) }
    }

    pub fn items(&self) -> &[String] {
        &self.items
    }

    /// Moves `emoji` to the front; does not persist.
    pub fn push(&mut self, emoji: &str) {
        if emoji.is_empty() {
            return;
        }
        self.items.retain(|e| e != emoji);
        self.items.insert(0, emoji.to_string());
        self.items.truncate(MAX_RECENTS);
    }

    /// Writes via a temp file and rename so a crash never leaves a torn file.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = tmp_path(path);
        std::fs::write(&tmp, serde_json::to_vec(&self.items)?)?;
        std::fs::rename(&tmp, path)
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionChip {
    pub emoji: String,
    pub count: u32,
    pub mine: bool,
}

/// Merges duplicate emoji, then sorts by count descending; ties keep first-seen order.
pub fn aggregate_label(reactions: &[(String, u32, bool)]) -> Vec<ReactionChip> {
    let mut chips: Vec<ReactionChip> = Vec::new();
    for (emoji, count, mine) in reactions {
        if *count == 0 {
            continue;
        }
        match chips.iter_mut().find(|c| &c.emoji == emoji) {
            Some(c) => {
                c.count += count;
                c.mine |= mine;
            }
            None => chips.push(ReactionChip { emoji: emoji.clone(), count: *count, mine: *mine }),
        }
    }
    chips.sort_by(|a, b| b.count.cmp(&a.count));
    chips
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("waclient-emoji-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("recents.json")
    }

    #[test]
    fn dataset_is_sizeable_and_well_formed() {
        let total: usize = categories().iter().map(|c| c.emoji.len()).sum();
        assert!(total >= 400, "only {total} emoji");
        assert_eq!(categories().len(), 8);
        assert!(categories().iter().all(|c| !c.emoji.is_empty()));
    }

    #[test]
    fn search_is_case_insensitive_and_ranks_prefix_first() {
        let r = search("  HeArT ");
        assert!(r.iter().any(|e| e.ch == "😍"));
        assert!(r.iter().any(|e| e.ch == "❤️"));
        // "art" only matches "heart" as a substring; prefix hits (art, arts) rank first.
        let r = search("art");
        assert!(r[0].keywords.split(' ').any(|w| w.starts_with("art")));
        assert!(r.iter().any(|e| e.ch == "❤️"));
    }

    #[test]
    fn search_empty_and_miss_and_cap() {
        assert!(search("").is_empty());
        assert!(search("zzzzqqq").is_empty());
        assert_eq!(search("a").len(), MAX_SEARCH_RESULTS);
    }

    #[test]
    fn recents_mru_dedup_and_cap() {
        let mut r = Recents::default();
        r.push("😀");
        r.push("😂");
        r.push("😀");
        assert_eq!(r.items(), ["😀", "😂"]);
        for i in 0..40 {
            r.push(&format!("e{i}"));
        }
        assert_eq!(r.items().len(), MAX_RECENTS);
        assert_eq!(r.items()[0], "e39");
    }

    #[test]
    fn recents_persist_roundtrip() {
        let p = temp_file("roundtrip");
        let mut r = Recents::load(&p);
        assert!(r.items().is_empty());
        r.push("👍");
        r.push("🔥");
        r.save().unwrap();
        r.push("🙏");
        r.save().unwrap();
        assert_eq!(Recents::load(&p).items(), ["🙏", "🔥", "👍"]);
        assert!(!tmp_path(&p).exists());
    }

    #[test]
    fn recents_load_tolerates_corruption() {
        let p = temp_file("corrupt");
        std::fs::write(&p, b"{not json").unwrap();
        let mut r = Recents::load(&p);
        assert!(r.items().is_empty());
        r.push("😀");
        r.save().unwrap();
        assert_eq!(Recents::load(&p).items(), ["😀"]);
    }

    #[test]
    fn aggregate_sorts_by_count_then_first_seen() {
        let input = vec![
            ("😂".to_string(), 1, false),
            ("👍".to_string(), 3, true),
            ("❤️".to_string(), 1, false),
            ("😂".to_string(), 1, true),
            ("🙏".to_string(), 0, false),
        ];
        let out = aggregate_label(&input);
        let got: Vec<_> = out.iter().map(|c| (c.emoji.as_str(), c.count, c.mine)).collect();
        assert_eq!(got, [("👍", 3, true), ("😂", 2, true), ("❤️", 1, false)]);
    }
}
