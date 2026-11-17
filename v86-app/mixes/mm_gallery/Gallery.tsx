// Swipe through every synced screen, one full page each, to pick favourites.
// Each page shows its number and name; open with exp://<your-ip>:8081/--/gallery
import { useFonts } from 'expo-font';
import { FlatList, Text, View, useWindowDimensions } from 'react-native';
import { screens } from '../mm_figma';
import { mixFonts } from '../mm_figma/fonts';

const ENTRIES = Object.entries(screens);

// "G042NightRings" -> "#042 Night Rings"
const label = (name: string) => name.replace(/^G(\d+)/, '#$1 ').replace(/([a-z])([A-Z0-9])/g, '$1 $2');

export default function Gallery() {
  const [ready, error] = useFonts(mixFonts);
  const { width, height } = useWindowDimensions();
  if (!ready && !error) return null;
  return (
    <FlatList
      data={ENTRIES}
      keyExtractor={([name]) => name}
      pagingEnabled
      showsVerticalScrollIndicator={false}
      // Only the page on screen and its neighbours are drawn.
      initialNumToRender={1}
      maxToRenderPerBatch={2}
      windowSize={3}
      removeClippedSubviews
      getItemLayout={(_, index) => ({ length: height, offset: height * index, index })}
      renderItem={({ item: [name, Screen], index }) => (
        <View style={{ width, height, overflow: 'hidden' }}>
          {/* Prototype links work here too: a tap opens the linked screen, and
              Back returns to the gallery. A swipe cancels the tap, so paging still works. */}
          <View style={{ flex: 1 }}>
            <Screen />
          </View>
          <View pointerEvents="none" className="absolute bottom-10 left-4 rounded-full bg-card px-3 py-1.5">
            <Text className="text-text text-xs font-semibold">
              {label(name)} · {index + 1}/{ENTRIES.length}
            </Text>
          </View>
        </View>
      )}
    />
  );
}
