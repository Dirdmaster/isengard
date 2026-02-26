import satori from 'satori'
import { Resvg } from '@resvg/resvg-js'
import { writeFileSync } from 'node:fs'
import { resolve } from 'node:path'

// Fetch Inter font (woff format, supported by satori)
const fontResponse = await fetch(
  'https://fonts.gstatic.com/s/inter/v18/UcCO3FwrK3iLTeHuS_nVMrMxCp50SjIw2boKoduKmMEVuLyfMZhrib2Bg-4.ttf',
)
const fontData = Buffer.from(await fontResponse.arrayBuffer())

const svg = await satori(
  {
    type: 'div',
    props: {
      style: {
        width: '1200px',
        height: '630px',
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        backgroundColor: '#1a1610',
        padding: '60px 80px',
        fontFamily: 'Inter',
      },
      children: [
        {
          type: 'div',
          props: {
            style: {
              fontSize: '14px',
              fontWeight: 500,
              letterSpacing: '0.2em',
              color: '#b38a2e',
              marginBottom: '28px',
            },
            children: 'CONTAINER AUTO-UPDATER',
          },
        },
        {
          type: 'div',
          props: {
            style: {
              fontSize: '80px',
              fontWeight: 700,
              color: '#d4a84b',
              letterSpacing: '-0.02em',
              lineHeight: 1,
              marginBottom: '24px',
            },
            children: 'Isengard',
          },
        },
        {
          type: 'div',
          props: {
            style: {
              fontSize: '24px',
              fontWeight: 400,
              color: '#b0a899',
              marginBottom: '20px',
            },
            children: 'The tower that never sleeps.',
          },
        },
        {
          type: 'div',
          props: {
            style: {
              fontSize: '18px',
              fontWeight: 400,
              color: '#807870',
              textAlign: 'center',
              maxWidth: '700px',
              lineHeight: 1.5,
            },
            children:
              'Lightweight, zero-config Docker container auto-updater with registry-first digest detection.',
          },
        },
      ],
    },
  },
  {
    width: 1200,
    height: 630,
    fonts: [
      {
        name: 'Inter',
        data: fontData,
        weight: 400,
        style: 'normal',
      },
      {
        name: 'Inter',
        data: fontData,
        weight: 700,
        style: 'normal',
      },
    ],
  },
)

const resvg = new Resvg(svg, {
  fitTo: { mode: 'width', value: 1200 },
})

const png = resvg.render().asPng()
const outPath = resolve(import.meta.dirname, '../public/og.png')
writeFileSync(outPath, png)
console.log(`OG image generated: ${outPath} (${(png.length / 1024).toFixed(1)} KB)`)
